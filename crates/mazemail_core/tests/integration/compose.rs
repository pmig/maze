//! Writing and sending mail over the in-memory IMAP server and a fake SMTP mailer:
//! SMTP servers are derived for identities, replies and forwards are prefilled from
//! stored mail, drafts are kept, sending builds `multipart/alternative`, records the
//! send and uploads the Sent copy, refusals come back typed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use auth::MemorySecretStore;
use imap::fake::{FakeMessage, FakeServer};
use mail_parser::{MessageParser, MimeHeaders as _};
use mazemail_core::{
    CoreHandle, Draft, DraftAttachment, DraftOrigin, Error, ReplyKind, SendResult, SyncEngineConfig,
};
use mazemail_types::{Address, Flags, FolderRole, MessageId};
use smtp::{Credentials, Mailer, OutboxEnvelope, SmtpConfig, SmtpError};
use store::{MessageScope, OutboxState, Queries as _, SmtpAuth, SmtpSecurity};
use vault::MemoryKeyStore;

use crate::accounts::{FakeConnector, PASSWORD, input, timing, wait_for};
use crate::common;

/// One submission the fake mailer received.
#[derive(Clone)]
struct Submission {
    config: SmtpConfig,
    user: String,
    envelope: OutboxEnvelope,
    raw: Vec<u8>,
}

/// Accepts everything, or answers with the configured error.
#[derive(Default)]
struct FakeMailer {
    sent: Mutex<Vec<Submission>>,
    fail: Mutex<Option<SmtpError>>,
}

impl std::fmt::Debug for FakeMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FakeMailer")
    }
}

impl FakeMailer {
    fn submissions(&self) -> Vec<Submission> {
        self.sent.lock().unwrap().clone()
    }

    fn fail_with(&self, error: Option<SmtpError>) {
        *self.fail.lock().unwrap() = error;
    }
}

#[async_trait]
impl Mailer for FakeMailer {
    async fn send(
        &self,
        config: &SmtpConfig,
        credentials: &Credentials,
        envelope: &OutboxEnvelope,
        raw: &[u8],
    ) -> Result<(), SmtpError> {
        if let Some(error) = self.fail.lock().unwrap().clone() {
            return Err(error);
        }
        let Credentials::Password { user, password } = credentials else {
            return Err(SmtpError::AuthFailed {
                message: "password expected".into(),
            });
        };
        if password.expose() != PASSWORD {
            return Err(SmtpError::AuthFailed {
                message: "535 5.7.8 bad password".into(),
            });
        }
        self.sent.lock().unwrap().push(Submission {
            config: config.clone(),
            user: user.clone(),
            envelope: envelope.clone(),
            raw: raw.to_vec(),
        });
        Ok(())
    }

    async fn check(&self, _: &SmtpConfig, credentials: &Credentials) -> Result<(), SmtpError> {
        match credentials {
            Credentials::Password { password, .. } if password.expose() == PASSWORD => Ok(()),
            _ => Err(SmtpError::AuthFailed {
                message: "535 5.7.8 bad password".into(),
            }),
        }
    }
}

struct Setup {
    core: CoreHandle,
    server: FakeServer,
    mailer: Arc<FakeMailer>,
    _dir: tempfile::TempDir,
}

async fn open(server: &FakeServer) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let secrets = Arc::new(MemorySecretStore::new());
    let mailer = Arc::new(FakeMailer::default());
    let engine = SyncEngineConfig::new(secrets)
        .with_connector(Arc::new(FakeConnector {
            server: server.clone(),
            reachable: true,
        }))
        .with_timing(timing())
        .with_mailer(mailer.clone());
    let core = CoreHandle::open(common::config(dir.path(), &keys).with_sync_engine(engine))
        .await
        .unwrap();
    Setup {
        core,
        server: server.clone(),
        mailer,
        _dir: dir,
    }
}

const ORIGINAL: &str = "From: Anna Example <anna@example.com>\r\n\
    To: Alice <alice@example.org>, Bob <bob@example.net>\r\n\
    Cc: carol@example.com\r\n\
    Subject: Q3 offsite\r\n\
    Message-ID: <q3@example.com>\r\n\
    In-Reply-To: <root@example.com>\r\n\
    References: <root@example.com>\r\n\
    Date: Thu, 22 Oct 2026 10:12:00 +0000\r\n\
    MIME-Version: 1.0\r\n\
    Content-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
    --b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nShall we meet on Thursday?\r\n\
    --b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
    <p>Shall we meet on <b>Thursday</b>?</p><img src=\"https://t.example/p.gif\">\r\n\
    --b--\r\n";

/// Adds alice's account and waits until the original is synced with its body.
async fn account_with_original(s: &Setup) -> MessageId {
    s.server.deliver("INBOX", FakeMessage::new(ORIGINAL));
    let info = s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    let core = s.core.clone();
    let inbox = move || {
        let reader = core.store().reader().unwrap();
        let folder = reader
            .folders_with_role(FolderRole::Inbox, Some(info.id))
            .unwrap()
            .into_iter()
            .next()?;
        reader
            .message_ids_in_scope(&MessageScope::Folder(folder.id))
            .unwrap()
            .into_iter()
            .next()
    };
    wait_for("the original", || inbox().is_some()).await;
    let message = inbox().unwrap();
    // Opening it fetches the body.
    let _ = s.core.message(message).await.unwrap();
    let core = s.core.clone();
    wait_for("the body", move || {
        core.store()
            .reader()
            .unwrap()
            .message(message)
            .unwrap()
            .is_some_and(|m| m.blob.is_some())
    })
    .await;
    message
}

#[tokio::test(flavor = "multi_thread")]
async fn new_accounts_get_an_smtp_server() {
    let server = FakeServer::new();
    let s = open(&server).await;
    let info = s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    let reader = s.core.store().reader().unwrap();
    let identity = reader.identities(Some(info.id)).unwrap().remove(0);
    let smtp = reader
        .smtp_server(identity.input.smtp.expect("an SMTP server"))
        .unwrap()
        .unwrap();
    assert_eq!(
        (smtp.host.as_str(), smtp.port, smtp.security, smtp.auth),
        (
            "smtp.example.org",
            Some(587),
            SmtpSecurity::StartTls,
            SmtpAuth::Plain
        )
    );
    assert_eq!(smtp.username.as_deref(), Some("alice@example.org"));
    drop(reader);

    // Testing SMTP logs in with the account's password; saves nothing.
    let tested = s.core.test_smtp_account(input(PASSWORD)).await.unwrap();
    assert_eq!(tested.host, "smtp.example.org");
    assert!(s.core.test_smtp_account(input("nope")).await.is_err());

    // Removing the account removes its server.
    s.core.remove_account(info.id).await.unwrap();
    assert!(
        s.core
            .store()
            .reader()
            .unwrap()
            .smtp_servers()
            .unwrap()
            .is_empty()
    );
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn replies_and_forwards_are_prefilled() {
    let server = FakeServer::new();
    let s = open(&server).await;
    let message = account_with_original(&s).await;
    let identities = s.core.identities().unwrap();
    assert_eq!(identities.len(), 1);
    let alice = identities[0].id;

    let reply = s
        .core
        .reply_draft(message, ReplyKind::Sender)
        .await
        .unwrap();
    assert_eq!(reply.identity, Some(alice));
    assert_eq!(
        reply.to,
        vec![Address::new(Some("Anna Example"), "anna@example.com")]
    );
    assert!(reply.cc.is_empty());
    assert_eq!(reply.subject, "Re: Q3 offsite");
    assert_eq!(reply.in_reply_to.as_deref(), Some("q3@example.com"));
    assert_eq!(reply.references, vec!["root@example.com", "q3@example.com"]);
    assert_eq!(reply.origin, Some(DraftOrigin::Reply(message)));
    let body = &reply.body_markdown;
    assert!(body.starts_with("\n\n"), "{body:?}");
    assert!(
        body.contains("Anna Example <anna@example.com> wrote:"),
        "{body}"
    );
    // The HTML part, as Markdown, quoted; no tracking image.
    assert!(body.contains("> Shall we meet on **Thursday**?"), "{body}");
    assert!(!body.contains("t.example"));

    let all = s.core.reply_draft(message, ReplyKind::All).await.unwrap();
    let emails = |list: &[Address]| list.iter().map(|a| a.email.clone()).collect::<Vec<_>>();
    assert_eq!(emails(&all.to), ["anna@example.com", "bob@example.net"]);
    assert_eq!(emails(&all.cc), ["carol@example.com"]);
    assert_eq!(all.origin, Some(DraftOrigin::ReplyAll(message)));

    let forward = s.core.forward_draft(message).await.unwrap();
    assert_eq!(forward.subject, "Fwd: Q3 offsite");
    assert!(forward.to.is_empty());
    assert!(
        forward
            .body_markdown
            .contains("-------- Forwarded Message --------")
    );
    assert!(
        forward
            .body_markdown
            .contains("From: Anna Example <anna@example.com>")
    );
    assert!(
        forward
            .body_markdown
            .contains("Shall we meet on **Thursday**?")
    );
    assert_eq!(forward.references, vec!["q3@example.com"]);
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn drafts_are_kept_and_sending_delivers_once_with_a_sent_copy() {
    let server = FakeServer::new();
    let s = open(&server).await;
    let message = account_with_original(&s).await;
    let mut draft = s.core.reply_draft(message, ReplyKind::All).await.unwrap();
    draft.body_markdown = format!("Thursday works, **see you**!{}", draft.body_markdown);
    draft.bcc = vec![Address::new(None::<String>, "boss@example.org")];
    let attachment = s._dir.path().join("agenda.txt");
    std::fs::write(&attachment, "1. Coffee\n").unwrap();
    draft.attachments.push(DraftAttachment {
        path: attachment,
        name: "agenda.txt".into(),
        size: 10,
    });

    let id = s.core.create_draft(draft.clone()).await.unwrap();
    assert_eq!(s.core.draft(id).unwrap(), draft);
    assert_eq!(s.core.drafts().unwrap().len(), 1);

    let result = s.core.send_draft(Some(id), draft.clone()).await.unwrap();
    assert_eq!(result, SendResult::Sent);
    // The draft is gone once the message is sent.
    assert!(s.core.drafts().unwrap().is_empty());

    let submissions = s.mailer.submissions();
    assert_eq!(submissions.len(), 1, "sent exactly once");
    let sent = &submissions[0];
    assert_eq!(sent.config.host, "smtp.example.org");
    assert_eq!(sent.config.port, 587);
    assert_eq!(sent.user, "alice@example.org");
    assert_eq!(sent.envelope.from, "alice@example.org");
    assert_eq!(
        sent.envelope.to,
        [
            "anna@example.com",
            "bob@example.net",
            "carol@example.com",
            "boss@example.org"
        ]
    );
    let parsed = MessageParser::default().parse(&sent.raw).unwrap();
    let message_id = parsed.message_id().unwrap().to_owned();
    assert!(message_id.ends_with("@example.org"), "{message_id}");
    assert_eq!(parsed.in_reply_to().as_text(), Some("q3@example.com"));
    assert_eq!(
        parsed.references().as_text_list().unwrap(),
        ["root@example.com", "q3@example.com"]
    );
    assert!(parsed.header("Bcc").is_none());
    assert!(!String::from_utf8_lossy(&sent.raw).contains("boss@example.org"));
    let root = parsed.root_part().content_type().unwrap();
    assert_eq!(root.subtype(), Some("mixed"));
    assert!(parsed.body_text(0).unwrap().contains("**see you**"));
    assert!(
        parsed
            .body_html(0)
            .unwrap()
            .contains("<strong>see you</strong>")
    );
    assert_eq!(
        parsed.attachment(0).unwrap().attachment_name(),
        Some("agenda.txt")
    );

    // The send log has it, the outbox says sent.
    assert!(
        s.core
            .store()
            .sendlog()
            .was_sent(&message_id)
            .unwrap()
            .is_some()
    );
    let outbox = s.core.store().reader().unwrap().outbox().unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].state, OutboxState::Sent);

    // A plain IMAP server doesn't file the Sent copy itself: Maze uploads it.
    wait_for("the Sent copy", || server.messages("Sent").len() == 1).await;
    assert_eq!(server.messages("Sent")[0].flags, Flags::SEEN);
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_typed_and_offline_mail_waits() {
    let server = FakeServer::new();
    let s = open(&server).await;
    s.core.add_imap_account(input(PASSWORD)).await.unwrap();
    let draft = Draft {
        to: vec![Address::new(None::<String>, "anna@example.com")],
        subject: "Hello".into(),
        body_markdown: "Hi".into(),
        ..s.core.new_draft().unwrap()
    };
    assert!(draft.identity.is_some());

    // No recipients.
    let empty = Draft {
        to: Vec::new(),
        ..draft.clone()
    };
    assert!(matches!(
        s.core.send_draft(None, empty).await,
        Err(Error::Compose {
            kind: mazemail_core::ComposeErrorKind::NoRecipients,
            ..
        })
    ));

    // Exchange refuses the From address: a typed error, the draft stays, nothing waits.
    s.mailer.fail_with(Some(SmtpError::from_reply(
        554,
        "5.2.252 SendAsDenied; not allowed to send as this address",
    )));
    let id = s.core.create_draft(draft.clone()).await.unwrap();
    let err = s
        .core
        .send_draft(Some(id), draft.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Send(SmtpError::SendAsDenied { .. })),
        "{err}"
    );
    assert!(err.to_string().contains("Send As"));
    assert_eq!(s.core.drafts().unwrap().len(), 1);
    assert!(
        s.core
            .store()
            .reader()
            .unwrap()
            .outbox()
            .unwrap()
            .is_empty()
    );

    // Offline: queued for later, the draft is gone.
    s.mailer.fail_with(Some(SmtpError::Connection {
        message: "connection refused".into(),
    }));
    let result = s.core.send_draft(Some(id), draft.clone()).await.unwrap();
    assert!(matches!(result, SendResult::Queued { .. }), "{result:?}");
    assert!(s.core.drafts().unwrap().is_empty());
    let outbox = s.core.store().reader().unwrap().outbox().unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].state, OutboxState::Queued);
    assert!(s.mailer.submissions().is_empty());
    s.core.shutdown().await.unwrap();
}

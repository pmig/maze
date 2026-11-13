//! Writing and sending mail over the in-memory IMAP server and a fake SMTP mailer:
//! SMTP servers are derived for identities, replies and forwards are prefilled from
//! stored mail, drafts are kept, sending builds `multipart/alternative`, records the
//! send and uploads the Sent copy, refusals come back typed.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use auth::MemorySecretStore;
use imap::fake::{FakeMessage, FakeServer};
use imap::{BodyPart, BodySummary};
use mail_parser::{MessageParser, TextList};
use mazemail_core::{
    CoreHandle, Draft, DraftAttachment, DraftOrigin, Error, ReplyKind, SendResult, SyncEngineConfig,
};
use mazemail_types::{Address, Flags, FolderRole, MessageId, Sensitive};
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
    open_with_settings(server, None).await
}

/// With a settings file holding `settings`.
async fn open_with_settings(server: &FakeServer, settings: Option<&str>) -> Setup {
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
    let mut config = common::config(&dir.path().join("data"), &keys).with_sync_engine(engine);
    if let Some(settings) = settings {
        let file = dir.path().join("settings.json");
        std::fs::write(&file, settings).unwrap();
        config = config.with_settings_file(file);
    }
    let core = CoreHandle::open(config).await.unwrap();
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
    let message = account_with(s, ORIGINAL).await;
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

/// An account whose inbox has `raw`, synced (its body may not be stored yet).
async fn account_with(s: &Setup, raw: &'static str) -> MessageId {
    account_with_message(s, FakeMessage::new(raw)).await
}

/// An account whose inbox has `message`, synced (its body may not be stored yet).
async fn account_with_message(s: &Setup, message: FakeMessage) -> MessageId {
    s.server.deliver("INBOX", message);
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
    inbox().unwrap()
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

/// A report with a PDF, and an HTML body with an inline logo. From 2020: older than the
/// offline policy's 90 days, so sync never downloads its body by itself.
const WITH_ATTACHMENTS: &str = "From: Anna Example <anna@example.com>\r\n\
To: Alice <alice@example.org>\r\n\
Subject: Q3 report\r\n\
Message-ID: <report@example.com>\r\n\
Date: Tue, 20 Oct 2020 09:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/related; boundary=\"inner\"\r\n\
\r\n\
--inner\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>The report is attached.</p><img src=\"cid:logo@example.com\">\r\n\
--inner\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@example.com>\r\n\
Content-Disposition: inline\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--inner--\r\n\
--outer\r\n\
Content-Type: application/pdf; name=\"report.pdf\"\r\n\
Content-Disposition: attachment; filename=\"report.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQgcmVwb3J0\r\n\
--outer--\r\n";

/// [`WITH_ATTACHMENTS`] with the BODYSTRUCTURE a server reports for it (the fake's
/// default is one `text/plain` part), so sync knows it has an attachment before the
/// body is downloaded.
fn with_attachments() -> FakeMessage {
    let part = |section: &str, mime_type: &str, subtype: &str, encoding: &str| BodyPart {
        section: section.into(),
        mime_type: mime_type.into(),
        subtype: subtype.into(),
        charset: None,
        encoding: encoding.into(),
        size: 0,
        disposition: None,
        filename: None,
        content_id: None,
        is_attachment: false,
    };
    FakeMessage::new(WITH_ATTACHMENTS)
        .internal_date(time::macros::datetime!(2020-10-20 09:00 UTC))
        .body(BodySummary {
            parts: vec![
                BodyPart {
                    charset: Some("utf-8".into()),
                    size: 68,
                    ..part("1.1", "text", "html", "7bit")
                },
                BodyPart {
                    size: 12,
                    disposition: Some("inline".into()),
                    content_id: Some("logo@example.com".into()),
                    ..part("1.2", "image", "png", "base64")
                },
                BodyPart {
                    size: 20,
                    disposition: Some("attachment".into()),
                    filename: Some(Sensitive::new("report.pdf".into())),
                    is_attachment: true,
                    ..part("2", "application", "pdf", "base64")
                },
            ],
        })
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_carry_the_attachments_along() {
    let server = FakeServer::new();
    let s = open(&server).await;
    // Not opened, and too old for sync to download: forwarding downloads the full
    // message itself.
    let message = account_with_message(&s, with_attachments()).await;
    let stored = s
        .core
        .store()
        .reader()
        .unwrap()
        .message(message)
        .unwrap()
        .unwrap();
    assert!(stored.has_attachments);
    assert!(stored.blob.is_none(), "not downloaded before the forward");

    let mut forward = s.core.forward_draft(message).await.unwrap();
    // The PDF, not the logo inside the HTML body.
    assert_eq!(forward.attachments.len(), 1, "{:?}", forward.attachments);
    let pdf = &forward.attachments[0];
    assert_eq!(pdf.name, "report.pdf");
    assert_eq!(pdf.size, 15);
    assert_eq!(
        pdf.path,
        std::path::PathBuf::new(),
        "never written to a file"
    );
    assert_eq!(pdf.forwarded.as_ref().unwrap().message, message);
    assert!(forward.body_markdown.contains("The report is attached."));

    // Kept with the draft, and read from the stored original when it is sent.
    forward.to = vec![Address::new(None::<String>, "bob@example.net")];
    let id = s.core.create_draft(forward.clone()).await.unwrap();
    assert_eq!(s.core.draft(id).unwrap(), forward);
    let result = s.core.send_draft(Some(id), forward).await.unwrap();
    assert_eq!(result, SendResult::Sent);
    let submissions = s.mailer.submissions();
    let parsed = MessageParser::default().parse(&submissions[0].raw).unwrap();
    let attachments: Vec<_> = parsed.attachments().collect();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].attachment_name(), Some("report.pdf"));
    assert_eq!(attachments[0].decoded().as_ref(), b"%PDF-1.4 report");
    let content_type = attachments[0].content_type().unwrap();
    assert_eq!(
        (content_type.ctype(), content_type.subtype()),
        ("application", Some("pdf"))
    );
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
        forwarded: None,
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
    let ids = |list: Option<TextList<'_>>| -> Vec<String> {
        list.map(|l| l.iter().map(str::to_owned).collect())
            .unwrap_or_default()
    };
    assert_eq!(ids(parsed.in_reply_to()), ["q3@example.com"]);
    assert_eq!(
        ids(parsed.references()),
        ["root@example.com", "q3@example.com"]
    );
    assert!(!parsed.headers().contains("Bcc"));
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
        parsed.attachments().next().unwrap().attachment_name(),
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
async fn the_footer_and_sent_with_maze_go_between_the_text_and_the_quote() {
    let server = FakeServer::new();
    let s = open_with_settings(
        &server,
        Some(r#"{ "compose": { "footer": "Alice · **Example Org**" } }"#),
    )
    .await;
    let message = account_with_original(&s).await;
    let alice = s.core.identities().unwrap()[0].id;
    let footer = s.core.compose_footer(Some(alice));
    assert_eq!(footer.footer, "Alice · **Example Org**");
    assert!(footer.support_maze, "on by default");

    let mut draft = s
        .core
        .reply_draft(message, ReplyKind::Sender)
        .await
        .unwrap();
    assert!(draft.support_maze);
    assert!(
        !draft.body_markdown.contains("Example Org"),
        "never in the body"
    );
    draft.body_markdown = format!("Thursday works.{}", draft.body_markdown);
    let id = s.core.create_draft(draft.clone()).await.unwrap();

    s.core.send_draft(Some(id), draft).await.unwrap();
    let submissions = s.mailer.submissions();
    let parsed = MessageParser::default().parse(&submissions[0].raw).unwrap();
    let text = parsed.body_text(0).unwrap().replace("\r\n", "\n");
    let mine = text.find("Thursday works.").unwrap();
    let footer = text.find("\n\nAlice · **Example Org**\n\n").unwrap();
    let maze = text.find("Sent with Maze (https://mazemail.dev)").unwrap();
    let quote = text.find("wrote:").unwrap();
    assert!(mine < footer && footer < maze && maze < quote, "{text}");
    let html = parsed.body_html(0).unwrap();
    let footer = html.find("<strong>Example Org</strong>").unwrap();
    let maze = html
        .find("Sent with <a href=\"https://mazemail.dev\"")
        .unwrap();
    assert!(
        footer < maze && maze < html.find("<blockquote>").unwrap(),
        "{html}"
    );

    // Only the setting decides: a draft saved with the old per-message flag off still
    // gets the line while `compose.support_maze` is on. The footer goes above a
    // forward's header too.
    let mut draft = s.core.forward_draft(message).await.unwrap();
    draft.to = vec![Address::new(None::<String>, "bob@example.net")];
    draft.support_maze = false;
    s.core.send_draft(None, draft).await.unwrap();
    let submissions = s.mailer.submissions();
    let parsed = MessageParser::default().parse(&submissions[1].raw).unwrap();
    let text = parsed.body_text(0).unwrap().replace("\r\n", "\n");
    assert!(text.contains("Sent with Maze"), "{text}");
    let footer = text.find("Alice · **Example Org**").unwrap();
    assert!(footer < text.find("-------- Forwarded Message --------").unwrap());
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn support_maze_off_in_the_settings_drops_the_line() {
    let server = FakeServer::new();
    let s = open_with_settings(&server, Some(r#"{ "compose": { "support_maze": false } }"#)).await;
    let message = account_with_original(&s).await;
    assert!(!s.core.new_draft().unwrap().support_maze);
    let mut reply = s.core.reply_draft(message, ReplyKind::All).await.unwrap();
    assert!(!reply.support_maze);
    assert!(!s.core.forward_draft(message).await.unwrap().support_maze);
    // Sending follows the setting, whatever an older draft recorded.
    reply.support_maze = true;
    s.core.send_draft(None, reply).await.unwrap();
    let submissions = s.mailer.submissions();
    let parsed = MessageParser::default().parse(&submissions[0].raw).unwrap();
    assert!(!parsed.body_text(0).unwrap().contains("Sent with Maze"));
    assert!(!parsed.body_html(0).unwrap().contains("Sent with"));
    // A per-account override wins (accounts are keyed by their name).
    let account = s.core.store().reader().unwrap().accounts().unwrap()[0]
        .name
        .clone();
    let settings = s._dir.path().join("settings.json");
    std::fs::write(
        &settings,
        format!(
            r#"{{ "compose": {{ "support_maze": false }},
                  "accounts": {{ "{account}": {{ "support_maze": true, "footer": "Hi" }} }} }}"#
        ),
    )
    .unwrap();
    s.core.reload_settings();
    assert!(s.core.new_draft().unwrap().support_maze);
    let alice = s.core.identities().unwrap()[0].id;
    assert_eq!(s.core.compose_footer(Some(alice)).footer, "Hi");
    assert_eq!(s.core.compose_footer(None).footer, "");
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

#[tokio::test(flavor = "multi_thread")]
async fn the_personal_spelling_dictionary_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let core = CoreHandle::open(common::config(&dir.path().join("data"), &keys))
        .await
        .unwrap();
    assert!(core.spelling_words().unwrap().is_empty());
    core.add_spelling_word("Glasskube".into()).await.unwrap();
    core.add_spelling_word("Jänner".into()).await.unwrap();
    core.add_spelling_word("Glasskube".into()).await.unwrap();
    assert_eq!(core.spelling_words().unwrap(), ["Glasskube", "Jänner"]);
    assert!(matches!(
        core.add_spelling_word("two words".into()).await,
        Err(Error::InvalidCommand(_))
    ));
    assert!(core.remove_spelling_word("Jänner".into()).await.unwrap());
    assert!(!core.remove_spelling_word("Jänner".into()).await.unwrap());
    assert_eq!(core.spelling_words().unwrap(), ["Glasskube"]);
    core.shutdown().await.unwrap();

    let core = CoreHandle::open(common::config(&dir.path().join("data"), &keys))
        .await
        .unwrap();
    assert_eq!(core.spelling_words().unwrap(), ["Glasskube"]);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn addresses_you_write_to_are_offered_for_completion() {
    let server = FakeServer::new();
    let s = open(&server).await;
    let message = account_with_original(&s).await;
    assert!(s.core.complete_addresses("an", 8).unwrap().is_empty());

    let mut reply = s.core.reply_draft(message, ReplyKind::All).await.unwrap();
    reply.bcc = vec![
        Address::new(Some("The Boss"), "boss@example.org"),
        // Your own address isn't collected.
        Address::new(None::<String>, "Alice@Example.org"),
    ];
    s.core.send_draft(None, reply).await.unwrap();

    assert_eq!(
        s.core.complete_addresses("an", 8).unwrap(),
        [Address::new(Some("Anna Example"), "anna@example.com")]
    );
    // By name too, and every field counts (To, Cc and Bcc).
    assert_eq!(
        s.core.complete_addresses("boss", 8).unwrap(),
        [Address::new(Some("The Boss"), "boss@example.org")]
    );
    assert_eq!(
        s.core.complete_addresses("car", 8).unwrap(),
        [Address::new(None::<String>, "carol@example.com")]
    );
    assert!(s.core.complete_addresses("ali", 8).unwrap().is_empty());

    // Writing again ranks by how often: Bob twice now, before the Boss once.
    let mut again = s.core.new_draft().unwrap();
    again.to = vec![Address::new(Some("Bob"), "BOB@example.net")];
    s.core.send_draft(None, again).await.unwrap();
    let b: Vec<String> = s
        .core
        .complete_addresses("b", 8)
        .unwrap()
        .into_iter()
        .map(|a| a.email)
        .collect();
    assert_eq!(b, ["bob@example.net", "boss@example.org"]);

    // All of them in the collected book, once each.
    let contacts = s
        .core
        .store()
        .reader()
        .unwrap()
        .search_contacts("", 50)
        .unwrap();
    assert!(contacts.iter().all(|c| c.book == "collected"));
    assert_eq!(contacts.len(), 4, "anna, bob, carol and the boss");
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_sent_to_inbox_puts_a_read_copy_into_the_inbox() {
    let server = FakeServer::new();
    let s = open_with_settings(
        &server,
        Some(r#"{ "compose": { "copy_sent_to_inbox": true } }"#),
    )
    .await;
    let message = account_with_original(&s).await;
    let mut reply = s
        .core
        .reply_draft(message, ReplyKind::Sender)
        .await
        .unwrap();
    reply.body_markdown = format!("Thursday works.{}", reply.body_markdown);
    assert_eq!(
        s.core.send_draft(None, reply).await.unwrap(),
        SendResult::Sent
    );

    // The Sent copy, and the same message in the Inbox next to the original, read.
    wait_for("the Sent copy", || server.messages("Sent").len() == 1).await;
    wait_for("the Inbox copy", || server.messages("INBOX").len() == 2).await;
    let sent = &server.messages("Sent")[0];
    let copy = server
        .messages("INBOX")
        .into_iter()
        .find(|m| m.uid != 1)
        .unwrap();
    assert_eq!(copy.flags, Flags::SEEN);
    assert_eq!(
        server.raw("INBOX", copy.uid).unwrap(),
        server.raw("Sent", sent.uid).unwrap()
    );
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn aliases_from_the_settings_become_identities_to_send_from() {
    let server = FakeServer::new();
    let s = open_with_settings(&server, Some("{}")).await;
    let message = account_with_original(&s).await;
    let account = s.core.store().reader().unwrap().accounts().unwrap()[0]
        .name
        .clone();
    let settings = s._dir.path().join("settings.json");
    let write = |aliases: &str| {
        std::fs::write(
            &settings,
            format!(r#"{{ "accounts": {{ "{account}": {{ "aliases": {aliases} }} }} }}"#),
        )
        .unwrap();
        s.core.reload_settings();
    };
    let emails = |core: &mazemail_core::CoreHandle| -> Vec<String> {
        core.identities()
            .unwrap()
            .into_iter()
            .map(|i| i.email)
            .collect()
    };

    write(r#"["sales@example.org", { "email": "support@example.org", "name": "Support" }]"#);
    let core = s.core.clone();
    wait_for("the alias identities", move || emails(&core).len() == 3).await;
    assert_eq!(
        emails(&s.core),
        [
            "alice@example.org",
            "sales@example.org",
            "support@example.org"
        ],
        "the account's own address stays first and the default"
    );
    let support = s
        .core
        .identities()
        .unwrap()
        .into_iter()
        .find(|i| i.email == "support@example.org")
        .unwrap();
    assert_eq!(support.display_name.as_deref(), Some("Support"));
    assert!(!support.is_default);

    // Sending as the alias: its address in From and in the envelope, the account's
    // SMTP login.
    let mut reply = s
        .core
        .reply_draft(message, ReplyKind::Sender)
        .await
        .unwrap();
    reply.identity = Some(support.id);
    reply.body_markdown = format!("We're on it.{}", reply.body_markdown);
    assert_eq!(
        s.core.send_draft(None, reply).await.unwrap(),
        SendResult::Sent
    );
    let sent = &s.mailer.submissions()[0];
    assert_eq!(sent.envelope.from, "support@example.org");
    assert_eq!(sent.user, "alice@example.org");
    let parsed = MessageParser::default().parse(&sent.raw).unwrap();
    assert_eq!(
        parsed.from().unwrap().first().unwrap().address(),
        Some("support@example.org")
    );

    // Taken out of the settings, the alias goes; the account's address stays.
    write(r#"["sales@example.org"]"#);
    let core = s.core.clone();
    wait_for("the removed alias to go", move || emails(&core).len() == 2).await;
    assert_eq!(emails(&s.core), ["alice@example.org", "sales@example.org"]);
    s.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn aliases_can_be_added_and_removed_in_the_settings_file() {
    let server = FakeServer::new();
    let s = open_with_settings(&server, Some("{ \"font_size\": 15 }")).await;
    let _message = account_with_original(&s).await;
    let account = s.core.store().reader().unwrap().accounts().unwrap()[0].clone();
    let file = s._dir.path().join("settings.json");
    let emails = |core: &mazemail_core::CoreHandle| -> Vec<String> {
        core.identities()
            .unwrap()
            .into_iter()
            .map(|i| i.email)
            .collect()
    };

    s.core
        .set_alias(account.id, " sales@example.org ", None, true)
        .unwrap();
    s.core
        .set_alias(account.id, "support@example.org", Some("Support"), true)
        .unwrap();
    // Written next to what was there, and read back.
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written["font_size"], 15);
    assert_eq!(
        written["accounts"][account.name.as_str()]["aliases"],
        serde_json::json!(["sales@example.org", { "email": "support@example.org", "name": "Support" }])
    );
    let core = s.core.clone();
    wait_for("the alias identities", move || emails(&core).len() == 3).await;
    let listed: Vec<String> = s
        .core
        .aliases(account.id)
        .unwrap()
        .into_iter()
        .map(|a| a.email)
        .collect();
    assert_eq!(listed, ["sales@example.org", "support@example.org"]);

    // Adding an address again replaces its entry; removing takes it out.
    s.core
        .set_alias(account.id, "SALES@example.org", Some("Sales"), true)
        .unwrap();
    assert_eq!(s.core.aliases(account.id).unwrap().len(), 2);
    s.core
        .set_alias(account.id, "support@example.org", None, false)
        .unwrap();
    let core = s.core.clone();
    wait_for("the removed alias to go", move || emails(&core).len() == 2).await;
    // Not an address: refused, nothing written.
    assert!(s.core.set_alias(account.id, "nope", None, true).is_err());
    assert_eq!(s.core.aliases(account.id).unwrap().len(), 1);
    s.core.shutdown().await.unwrap();
}

//! Transcript tests: [`ImapClient`] against a scripted server over an in-memory pipe.
//!
//! A transcript is a list of lines:
//!
//! - `C: …` the exact line the client must send (without CRLF),
//! - `S: …` a line the server sends (CRLF appended),
//! - `L: …` literal bytes the client must send, followed by CRLF,
//! - `TLS` the server runs a TLS handshake (STARTTLS).
//!
//! In `S:` and `L:` lines the two-character sequence `\r\n` stands for CRLF, so
//! literals can be written inline.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, ensure};
use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

use super::*;
use crate::CertificateDer;
use crate::config::TlsVerification;
use crate::gmail::GmailLabel;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

#[derive(Debug)]
enum Line {
    Client(String),
    Server(String),
    Literal(Vec<u8>),
    Tls,
}

fn unescape(s: &str) -> String {
    s.replace("\\r\\n", "\r\n")
}

fn parse(transcript: &str) -> Vec<Line> {
    transcript
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.is_empty())
        .map(|l| {
            if l == "C:" {
                Line::Client(String::new())
            } else if let Some(rest) = l.strip_prefix("C: ") {
                Line::Client(rest.to_owned())
            } else if let Some(rest) = l.strip_prefix("S: ") {
                Line::Server(unescape(rest))
            } else if let Some(rest) = l.strip_prefix("L: ") {
                Line::Literal(unescape(rest).into_bytes())
            } else if l == "TLS" {
                Line::Tls
            } else {
                panic!("bad transcript line: {l}")
            }
        })
        .collect()
}

/// An IMAP literal for an `S:` line: `{n}\r\n<content>` with escaped CRLFs.
fn literal(content: &str) -> String {
    format!(
        "{{{}}}\\r\\n{}",
        content.len(),
        content.replace("\r\n", "\\r\\n")
    )
}

struct Tls {
    server: Arc<rustls::ServerConfig>,
    root: CertificateDer<'static>,
}

fn test_tls() -> Tls {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let root = cert.cert.der().clone();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![root.clone()], key)
        .unwrap();
    Tls {
        server: Arc::new(server),
        root,
    }
}

async fn serve(
    stream: tokio::io::DuplexStream,
    lines: Vec<Line>,
    tls: Option<Arc<rustls::ServerConfig>>,
    implicit_tls: bool,
) -> anyhow::Result<()> {
    let mut io: BufReader<Box<dyn Io>> = if implicit_tls {
        let acceptor = tokio_rustls::TlsAcceptor::from(tls.clone().context("no TLS config")?);
        BufReader::new(Box::new(acceptor.accept(stream).await?))
    } else {
        BufReader::new(Box::new(stream))
    };
    for (n, line) in lines.into_iter().enumerate() {
        match line {
            Line::Client(expected) => {
                let mut got = String::new();
                let read = io.read_line(&mut got).await?;
                ensure!(read > 0, "step {n}: client closed, expected {expected:?}");
                let got = got.trim_end_matches("\r\n");
                ensure!(
                    got == expected,
                    "step {n}: expected {expected:?}, got {got:?}"
                );
            }
            Line::Server(text) => {
                let w = io.get_mut();
                w.write_all(text.as_bytes()).await?;
                w.write_all(b"\r\n").await?;
                w.flush().await?;
            }
            Line::Literal(expected) => {
                let mut got = vec![0; expected.len() + 2];
                io.read_exact(&mut got).await?;
                ensure!(
                    got[..expected.len()] == expected[..],
                    "step {n}: literal differs"
                );
                ensure!(
                    &got[expected.len()..] == b"\r\n",
                    "step {n}: literal not ended by CRLF"
                );
            }
            Line::Tls => {
                ensure!(io.buffer().is_empty(), "step {n}: data buffered before TLS");
                let acceptor =
                    tokio_rustls::TlsAcceptor::from(tls.clone().context("no TLS config")?);
                let inner = io.into_inner();
                io = BufReader::new(Box::new(acceptor.accept(inner).await?));
            }
        }
    }
    Ok(())
}

fn config() -> ImapConfig {
    let mut config = ImapConfig::new("localhost", 143, ImapAuth::password("alice", "secret"))
        .with_security(Security::PlainInsecure)
        .with_command_timeout(Duration::from_secs(5))
        .with_connect_timeout(Duration::from_secs(5));
    config.client_id = None;
    config
}

/// Greeting and `AUTHENTICATE PLAIN` login, then `CAPABILITY` with `caps`.
/// With `QRESYNC` and `ENABLE` in `caps`, also `ENABLE QRESYNC` as `A0003`.
fn prelude(caps: &str) -> String {
    let mut out = format!(
        "S: * OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] Maze test server ready
         C: A0001 AUTHENTICATE PLAIN
         S: +
         C: AGFsaWNlAHNlY3JldA==
         S: A0001 OK authenticated
         C: A0002 CAPABILITY
         S: * CAPABILITY IMAP4rev1 {caps}
         S: A0002 OK done
         "
    );
    if caps.contains("QRESYNC") && caps.contains("ENABLE") {
        out.push_str(
            "C: A0003 ENABLE QRESYNC
             S: * ENABLED QRESYNC
             S: A0003 OK enabled
             ",
        );
    }
    out
}

struct Scripted {
    server: JoinHandle<anyhow::Result<()>>,
}

impl Scripted {
    async fn done(self) {
        self.server.await.unwrap().unwrap();
    }
}

async fn start(transcript: &str, config: &ImapConfig) -> (Result<ImapClient>, Scripted) {
    start_with_tls(transcript, config, None, false).await
}

async fn start_with_tls(
    transcript: &str,
    config: &ImapConfig,
    tls: Option<Arc<rustls::ServerConfig>>,
    implicit: bool,
) -> (Result<ImapClient>, Scripted) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(serve(server_io, parse(transcript), tls, implicit));
    let client = tokio::time::timeout(
        Duration::from_secs(10),
        ImapClient::connect_stream(client_io, config),
    )
    .await
    .expect("connect timed out");
    (client, Scripted { server })
}

async fn connected(transcript: &str) -> (ImapClient, Scripted) {
    let (client, server) = start(transcript, &config()).await;
    match client {
        Ok(client) => (client, server),
        Err(e) => {
            let server_result = server.server.await;
            panic!("connect failed: {e:?}; server: {server_result:?}");
        }
    }
}

const SELECTED_INBOX: &str = "
    C: A0003 SELECT \"INBOX\"
    S: * 3 EXISTS
    S: * OK [UIDVALIDITY 7] UIDs valid
    S: * OK [UIDNEXT 10] Predicted next UID
    S: A0003 OK [READ-WRITE] Select completed
";

#[tokio::test]
async fn login_with_plain_reads_capabilities_and_enables_qresync() {
    let script = format!(
        "{}C: A0004 LOGOUT
         S: * BYE Logging out
         S: A0004 OK Logout completed",
        prelude("IDLE MOVE UIDPLUS CONDSTORE QRESYNC ENABLE SPECIAL-USE X-GM-EXT-1")
    );
    let (mut client, server) = connected(&script).await;
    let caps = client.capabilities().await.unwrap();
    assert!(caps.idle() && caps.has_move() && caps.uidplus() && caps.qresync());
    assert!(caps.condstore() && caps.special_use() && caps.gmail());
    assert!(!caps.esearch());
    assert!(client.qresync_enabled());
    assert!(client.is_gmail());
    client.logout().await.unwrap();
    assert!(client.is_broken());
    assert!(matches!(client.noop().await, Err(ImapError::Broken)));
    server.done().await;
}

#[tokio::test]
async fn falls_back_to_login_and_sends_id() {
    let script = r#"
        S: * OK IMAP4rev1 ready
        C: A0001 CAPABILITY
        S: * CAPABILITY IMAP4rev1 ID
        S: A0001 OK done
        C: A0002 LOGIN "alice" "secret"
        S: A0002 OK logged in
        C: A0003 CAPABILITY
        S: * CAPABILITY IMAP4rev1 ID
        S: A0003 OK done
        C: A0004 ID ("name" "Maze" "version" "1.2.3")
        S: * ID ("name" "Test")
        S: A0004 OK id done
    "#;
    let mut config = config();
    config.client_id = Some(vec![
        ("name".into(), "Maze".into()),
        ("version".into(), "1.2.3".into()),
    ]);
    let (client, server) = start(script, &config).await;
    let client = client.unwrap();
    assert!(client.caps().id());
    server.done().await;
}

#[tokio::test]
async fn rejected_xoauth2_maps_to_auth_error() {
    let script = r#"
        S: * OK [CAPABILITY IMAP4rev1 AUTH=XOAUTH2] ready
        C: A0001 AUTHENTICATE XOAUTH2
        S: +
        C: dXNlcj1ib2JAZXhhbXBsZS5vcmcBYXV0aD1CZWFyZXIgdG9rZW4BAQ==
        S: + eyJzdGF0dXMiOiI0MDAifQ==
        C:
        S: A0001 NO [AUTHENTICATIONFAILED] Invalid credentials
    "#;
    let mut config = config();
    config.auth = ImapAuth::xoauth2("bob@example.org", "token");
    let (client, server) = start(script, &config).await;
    let err = client.unwrap_err();
    assert!(matches!(err, ImapError::Auth(_)), "{err:?}");
    assert!(!err.to_string().contains("token"));
    server.done().await;
}

#[tokio::test]
async fn xoauth2_requires_the_mechanism() {
    let script = "S: * OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] ready";
    let mut config = config();
    config.auth = ImapAuth::xoauth2("bob@example.org", "token");
    let (client, server) = start(script, &config).await;
    assert!(matches!(
        client.unwrap_err(),
        ImapError::Unsupported("AUTH=XOAUTH2")
    ));
    server.done().await;
}

#[tokio::test]
async fn bye_greeting_is_connection_lost() {
    let (client, server) = start("S: * BYE Too many connections", &config()).await;
    match client.unwrap_err() {
        ImapError::ConnectionLost(Some(text)) => assert_eq!(text, "Too many connections"),
        other => panic!("unexpected {other:?}"),
    }
    server.done().await;
}

#[tokio::test]
async fn list_maps_special_use_utf7_and_subscriptions() {
    let script = format!(
        r#"{}C: A0003 LIST "" "*" RETURN (SPECIAL-USE)
        S: * LIST (\HasNoChildren) "/" "INBOX"
        S: * LIST (\HasNoChildren \Sent) "/" "Sent"
        S: * LIST (\HasChildren \Noselect) "/" "[Gmail]"
        S: * LIST (\HasNoChildren \All) "/" "[Gmail]/All Mail"
        S: * LIST (\HasNoChildren \Important) "/" "[Gmail]/Important"
        S: * LIST (\HasNoChildren) "/" "Entw&APw-rfe"
        S: * LIST (\HasNoChildren \Junk) NIL Spam
        S: A0003 OK LIST completed
        C: A0004 LSUB "" "*"
        S: * LSUB () "/" "inbox"
        S: * LSUB () "/" "Entw&APw-rfe"
        S: A0004 OK LSUB completed
        C: A0005 SELECT "Entw&APw-rfe"
        S: * OK [UIDVALIDITY 1] ok
        S: A0005 OK [READ-WRITE] done
        "#,
        prelude("SPECIAL-USE LIST-EXTENDED")
    );
    let (mut client, server) = connected(&script).await;
    let folders = client.list().await.unwrap();
    let names: Vec<&str> = folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "INBOX",
            "Sent",
            "[Gmail]",
            "[Gmail]/All Mail",
            "[Gmail]/Important",
            "Entwürfe",
            "Spam"
        ]
    );
    let roles: Vec<Option<FolderRole>> = folders.iter().map(|f| f.role).collect();
    assert_eq!(
        roles,
        [
            Some(FolderRole::Inbox),
            Some(FolderRole::Sent),
            None,
            Some(FolderRole::All),
            Some(FolderRole::Important),
            None,
            Some(FolderRole::Junk)
        ]
    );
    assert!(!folders[2].selectable && folders[2].has_attribute("\\noselect"));
    assert!(folders[0].subscribed && folders[5].subscribed && !folders[1].subscribed);
    assert_eq!(folders[3].delimiter, Some('/'));
    assert_eq!(folders[3].leaf_name(), "All Mail");
    assert_eq!(folders[6].delimiter, None);
    assert_eq!(folders[5].raw_name, "Entw&APw-rfe");
    // Decoded names are mapped back to the server's names.
    client.select("Entwürfe", None).await.unwrap();
    server.done().await;
}

#[tokio::test]
async fn select_with_qresync_returns_vanished_and_changes() {
    let script = format!(
        r#"{}C: A0004 SELECT "INBOX" (QRESYNC (67890007 20050715194045000))
        S: * OK [CLOSED] Previous mailbox closed
        S: * 314 EXISTS
        S: * 0 RECENT
        S: * OK [UIDVALIDITY 67890007] UIDVALIDITY
        S: * OK [UIDNEXT 567] Predicted next UID
        S: * OK [HIGHESTMODSEQ 20050715194045319] Highest
        S: * FLAGS (\Answered \Flagged \Draft \Deleted \Seen $Junk)
        S: * OK [PERMANENTFLAGS (\Answered \Flagged \Draft \Deleted \Seen \*)] Limited
        S: * VANISHED (EARLIER) 41,43:116,118,120:211,214:540
        S: * 49 FETCH (UID 117 FLAGS (\Seen \Answered $label1) MODSEQ (20050715194045000))
        S: * 50 FETCH (UID 119 FLAGS (\Draft $MDNSent) MODSEQ (20050715194045308))
        S: A0004 OK [READ-WRITE] mailbox selected
        "#,
        prelude("CONDSTORE QRESYNC ENABLE")
    );
    let (mut client, server) = connected(&script).await;
    let info = client
        .select(
            "inbox",
            Some(QresyncArgs {
                uidvalidity: 67_890_007,
                highest_modseq: 20_050_715_194_045_000,
                known_uids: None,
            }),
        )
        .await
        .unwrap();
    assert!(info.qresync_applied);
    assert_eq!(info.exists, 314);
    assert_eq!(info.uidvalidity, 67_890_007);
    assert_eq!(info.uidnext, Some(567));
    assert_eq!(info.highest_modseq, Some(20_050_715_194_045_319));
    assert!(info.allows_new_keywords);
    assert!(!info.permanent_flags.contains(&"\\*".to_owned()));
    assert!(!info.read_only);
    assert_eq!(info.vanished.to_string(), "41,43:116,118,120:211,214:540");
    assert_eq!(info.changed.len(), 2);
    assert_eq!(info.changed[0].uid, 117);
    assert_eq!(info.changed[0].flags, Flags::SEEN | Flags::ANSWERED);
    assert_eq!(info.changed[0].keywords, ["$label1"]);
    assert_eq!(info.changed[1].modseq, Some(20_050_715_194_045_308));
    assert!(client.take_events().is_empty());
    server.done().await;
}

#[tokio::test]
async fn select_with_condstore_handles_nomodseq_and_read_only() {
    let script = format!(
        r#"{}C: A0003 SELECT "Shared" (CONDSTORE)
        S: * 2 EXISTS
        S: * OK [UIDVALIDITY 3] ok
        S: * OK [NOMODSEQ] Sorry, this mailbox format doesn't support modsequences
        S: * OK [PERMANENTFLAGS ()] No permanent flags
        S: A0003 OK [READ-ONLY] Shared selected
        C: A0004 SELECT "Gone" (CONDSTORE)
        S: A0004 NO [NONEXISTENT] Mailbox doesn't exist
        C: A0005 UID SEARCH ALL
        "#,
        prelude("CONDSTORE")
    );
    let (mut client, server) = connected(&script).await;
    // QRESYNC args are ignored without QRESYNC; CONDSTORE is used instead.
    let qresync = QresyncArgs {
        uidvalidity: 3,
        highest_modseq: 9,
        known_uids: None,
    };
    let info = client.select("Shared", Some(qresync)).await.unwrap();
    assert_eq!(info.highest_modseq, None);
    assert!(info.read_only && !info.allows_new_keywords && !info.qresync_applied);
    match client.select("Gone", None).await.unwrap_err() {
        ImapError::No { command, code, .. } => {
            assert_eq!(command, "SELECT");
            assert_eq!(code.as_deref(), Some("NONEXISTENT"));
        }
        other => panic!("unexpected {other:?}"),
    }
    // A failed SELECT deselects: message commands fail locally.
    assert!(matches!(
        client.uid_search_all().await,
        Err(ImapError::NoFolderSelected)
    ));
    assert!(!client.is_broken());
    drop(client);
    // The script expects one more command that never comes; the server sees EOF.
    assert!(server.server.await.unwrap().is_err());
}

#[tokio::test]
async fn fetch_changes_uses_changedsince_and_vanished() {
    let script = format!(
        r#"{}C: A0004 SELECT "INBOX" (CONDSTORE)
        S: * OK [UIDVALIDITY 7] ok
        S: * OK [HIGHESTMODSEQ 12400] ok
        S: A0004 OK [READ-WRITE] done
        C: A0005 UID FETCH 1:* (UID FLAGS MODSEQ) (CHANGEDSINCE 12345 VANISHED)
        S: * VANISHED (EARLIER) 1:2,5
        S: * 1 FETCH (UID 3 FLAGS (\Seen) MODSEQ (12350))
        S: * 2 FETCH (UID 4 FLAGS () MODSEQ (12346))
        S: A0005 OK Fetch completed
        "#,
        prelude("CONDSTORE QRESYNC ENABLE"),
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    let changes = client.fetch_changes(12_345).await.unwrap();
    assert_eq!(changes.vanished.to_string(), "1:2,5");
    assert_eq!(changes.changed.len(), 2);
    assert_eq!(changes.changed[0].uid, 3);
    assert_eq!(changes.changed[0].flags, Flags::SEEN);
    assert_eq!(changes.changed[1].flags, Flags::empty());
    assert_eq!(changes.changed[1].modseq, Some(12_346));
    server.done().await;
}

#[tokio::test]
async fn fetch_changes_requires_condstore() {
    let script = format!("{}{SELECTED_INBOX}", prelude("IDLE"));
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    assert!(matches!(
        client.fetch_changes(1).await,
        Err(ImapError::Unsupported("CONDSTORE"))
    ));
    server.done().await;
}

#[tokio::test]
async fn fetch_headers_parses_header_bodystructure_and_gmail() {
    let header = "From: a@example.org\r\nSubject: Hi\r\n\r\n";
    let script = format!(
        r#"{}C: A0003 SELECT "[Gmail]/All Mail" (CONDSTORE)
        S: * OK [UIDVALIDITY 11] ok
        S: * OK [HIGHESTMODSEQ 900] ok
        S: A0003 OK [READ-WRITE] done
        C: A0004 UID FETCH 5:* (UID FLAGS RFC822.SIZE INTERNALDATE BODYSTRUCTURE BODY.PEEK[HEADER] MODSEQ X-GM-MSGID X-GM-THRID X-GM-LABELS)
        S: * 1 FETCH (UID 4 FLAGS (\Seen) RFC822.SIZE 10 INTERNALDATE "01-Jan-2020 00:00:00 +0000" BODYSTRUCTURE ("TEXT" "PLAIN" ("CHARSET" "us-ascii") NIL NIL "7BIT" 3 1 NIL NIL NIL NIL) BODY[HEADER] {literal_header} MODSEQ (1) X-GM-MSGID 1 X-GM-THRID 1 X-GM-LABELS ())
        S: * 2 FETCH (UID 7 FLAGS (\Seen $Forwarded Work) RFC822.SIZE 4242 INTERNALDATE "17-Jul-1996 02:44:25 -0700" BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "utf-8") NIL NIL "QUOTED-PRINTABLE" 120 4 NIL NIL NIL NIL)("TEXT" "HTML" ("CHARSET" "utf-8") NIL NIL "BASE64" 300 5 NIL NIL NIL NIL) "ALTERNATIVE" ("BOUNDARY" "b1") NIL NIL NIL) BODY[HEADER] {literal_header} MODSEQ (812) X-GM-MSGID 1278455344230334865 X-GM-THRID 1266894439832287888 X-GM-LABELS (\Inbox \Important "&AMQ-pfel" Work/Projects))
        S: * 3 FETCH (UID 8 FLAGS () RFC822.SIZE 99 INTERNALDATE " 2-Feb-2021 10:00:00 +0100" BODYSTRUCTURE ((("TEXT" "PLAIN" ("CHARSET" "utf-8") NIL NIL "7BIT" 10 1 NIL NIL NIL NIL)("TEXT" "HTML" ("CHARSET" "utf-8") NIL NIL "7BIT" 20 1 NIL NIL NIL NIL) "ALTERNATIVE" ("BOUNDARY" "b2") NIL NIL NIL)("APPLICATION" "PDF" ("NAME" "invoice.pdf") NIL NIL "BASE64" 5000 NIL ("ATTACHMENT" ("FILENAME" "invoice.pdf")) NIL NIL) "MIXED" ("BOUNDARY" "b3") NIL NIL NIL) BODY[HEADER] {literal_header} MODSEQ (813) X-GM-MSGID 2 X-GM-THRID 2 X-GM-LABELS ("\\Sent" "\\Starred" "Say \"hi\""))
        S: * 2 FETCH (FLAGS (\Seen \Flagged))
        S: A0004 OK Fetch completed
        "#,
        prelude("CONDSTORE X-GM-EXT-1"),
        literal_header = literal(header),
    );
    let (mut client, server) = connected(&script).await;
    client.select("[Gmail]/All Mail", None).await.unwrap();
    let headers = client
        .fetch_headers(&UidSet::from_uid_onwards(5))
        .await
        .unwrap();
    // UID 4 is the `5:*` artifact (highest UID < 5) and is dropped.
    assert_eq!(headers.iter().map(|h| h.uid).collect::<Vec<_>>(), [7, 8]);
    let h = &headers[0];
    assert_eq!(h.header.expose().as_ref(), header.as_bytes());
    assert_eq!(h.flags, Flags::SEEN | Flags::FORWARDED);
    assert_eq!(h.keywords, ["Work"]);
    assert_eq!(h.size, 4242);
    assert_eq!(h.modseq, Some(812));
    assert_eq!(h.internal_date.unwrap().unix_timestamp(), 837_596_665);
    let gmail = h.gmail.as_ref().unwrap();
    assert_eq!(gmail.msg_id, 1_278_455_344_230_334_865);
    assert_eq!(gmail.thread_id, 1_266_894_439_832_287_888);
    assert_eq!(
        gmail.labels,
        [
            GmailLabel::Inbox,
            GmailLabel::Important,
            GmailLabel::User("Äpfel".into()),
            GmailLabel::User("Work/Projects".into())
        ]
    );
    let body = h.body.as_ref().unwrap();
    let sections: Vec<&str> = body.parts.iter().map(|p| p.section.as_str()).collect();
    assert_eq!(sections, ["1", "2"]);
    assert_eq!(body.parts[0].encoding, "quoted-printable");
    assert_eq!(body.parts[1].charset.as_deref(), Some("utf-8"));
    assert_eq!(body.text_parts().count(), 2);
    assert!(!body.has_attachments());

    // Gmail's real format: system labels as quoted strings with an escaped backslash.
    assert_eq!(
        headers[1].gmail.as_ref().unwrap().labels,
        [
            GmailLabel::Sent,
            GmailLabel::Starred,
            GmailLabel::User(r#"Say "hi""#.into())
        ]
    );
    let mixed = headers[1].body.as_ref().unwrap();
    let sections: Vec<&str> = mixed.parts.iter().map(|p| p.section.as_str()).collect();
    assert_eq!(sections, ["1.1", "1.2", "2"]);
    let pdf = &mixed.parts[2];
    assert!(pdf.is_attachment);
    assert_eq!(pdf.filename.as_ref().unwrap().expose(), "invoice.pdf");
    assert_eq!(pdf.disposition.as_deref(), Some("attachment"));
    assert_eq!(headers[1].internal_date.unwrap().day(), 2);
    assert!(mixed.has_attachments());
    // The flag update without the requested items is unsolicited.
    let events = client.take_events();
    assert!(matches!(
        &events[..],
        [SessionEvent::Flags { seq: 2, uid: None, flags, .. }] if *flags == Flags::SEEN | Flags::FLAGGED
    ));
    server.done().await;
}

#[tokio::test]
async fn fetch_part_and_raw() {
    let raw = "Subject: x\r\n\r\nbody";
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 UID FETCH 7 (UID BODY.PEEK[1.2])
        S: * 3 FETCH (UID 7 BODY[1.2] {part})
        S: A0004 OK done
        C: A0005 UID FETCH 7 (UID BODY.PEEK[])
        S: * 3 FETCH (UID 7 BODY[] {raw_lit})
        S: A0005 OK done
        C: A0006 UID FETCH 99 (UID BODY.PEEK[])
        S: A0006 OK done
        C: A0007 UID FETCH 7 (UID BODY.PEEK[9])
        S: * 3 FETCH (UID 7 BODY[9] NIL)
        S: A0007 OK done
        "#,
        prelude("IDLE"),
        part = literal("<p>hi</p>"),
        raw_lit = literal(raw),
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    assert_eq!(
        &client.fetch_part(7, "1.2").await.unwrap()[..],
        b"<p>hi</p>"
    );
    assert_eq!(&client.fetch_raw(7).await.unwrap()[..], raw.as_bytes());
    assert!(matches!(
        client.fetch_raw(99).await,
        Err(ImapError::MessageNotFound { uid: 99 })
    ));
    assert!(matches!(
        client.fetch_part(7, "9").await,
        Err(ImapError::PartNotFound { uid: 7, .. })
    ));
    assert!(matches!(
        client.fetch_part(7, "1 BODY[]").await,
        Err(ImapError::InvalidInput(_))
    ));
    server.done().await;
}

#[tokio::test]
async fn store_flags_and_labels() {
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 UID STORE 1:3,7 FLAGS.SILENT (\Seen \Flagged $Junk Work)
        S: A0004 OK Store completed
        C: A0005 UID STORE 9 -FLAGS.SILENT (\Seen)
        S: * 1 FETCH (UID 2 FLAGS (\Deleted) MODSEQ (5))
        S: A0005 OK Store completed
        C: A0006 UID STORE 9 +X-GM-LABELS (\Inbox "Work/&AMQ-")
        S: * 1 FETCH (X-GM-LABELS (\Inbox "Work/&AMQ-") UID 9)
        S: A0006 OK Store completed
        "#,
        prelude("X-GM-EXT-1")
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    let uids: UidSet = [1, 2, 3, 7].into_iter().collect();
    let op = FlagOp::set(Flags::SEEN | Flags::FLAGGED | Flags::JUNK).with_keywords(["Work"]);
    client.store_flags(&uids, &op).await.unwrap();
    client
        .store_flags(&UidSet::single(9), &FlagOp::remove(Flags::SEEN))
        .await
        .unwrap();
    // No-op add sends nothing.
    client
        .store_flags(&UidSet::single(9), &FlagOp::add(Flags::empty()))
        .await
        .unwrap();
    assert!(matches!(
        client
            .store_flags(
                &uids,
                &FlagOp::add(Flags::empty()).with_keywords(["bad word"])
            )
            .await,
        Err(ImapError::InvalidInput(_))
    ));
    client
        .store_gmail_labels(
            &UidSet::single(9),
            &LabelOp::add([GmailLabel::Inbox, GmailLabel::User("Work/Ä".into())]),
        )
        .await
        .unwrap();
    let events = client.take_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(
        events[0],
        SessionEvent::Flags {
            uid: Some(2),
            modseq: Some(5),
            ..
        }
    ));
    server.done().await;
}

#[tokio::test]
async fn gmail_labels_require_gmail() {
    let script = format!("{}{SELECTED_INBOX}", prelude("IDLE"));
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    assert!(matches!(
        client
            .store_gmail_labels(&UidSet::single(1), &LabelOp::archive())
            .await,
        Err(ImapError::Unsupported("X-GM-EXT-1"))
    ));
    server.done().await;
}

#[tokio::test]
async fn move_returns_copyuid_from_untagged_ok() {
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 UID MOVE 1:2 "Archive"
        S: * OK [COPYUID 432432 1:2 101:102] Moved UIDs.
        S: * 1 EXPUNGE
        S: * 1 EXPUNGE
        S: A0004 OK Move completed
        "#,
        prelude("MOVE UIDPLUS")
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    let copy = client
        .r#move(&UidSet::range(1, 2), "Archive")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copy.uidvalidity, 432_432);
    assert_eq!(copy.pairs().collect::<Vec<_>>(), [(1, 101), (2, 102)]);
    assert_eq!(
        client.take_events(),
        [SessionEvent::Expunge(1), SessionEvent::Expunge(1)]
    );
    server.done().await;
}

#[tokio::test]
async fn move_without_move_extension_copies_flags_and_expunges() {
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 UID COPY 5 "Trash"
        S: A0004 OK [COPYUID 9 5 77] Copy completed
        C: A0005 UID STORE 5 +FLAGS.SILENT (\Deleted)
        S: A0005 OK Store completed
        C: A0006 UID EXPUNGE 5
        S: * 2 EXPUNGE
        S: A0006 OK Expunge completed
        "#,
        prelude("UIDPLUS")
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    let copy = client.r#move(&UidSet::single(5), "Trash").await.unwrap();
    assert_eq!(copy.unwrap().destination_of(5), Some(77));
    server.done().await;
}

#[tokio::test]
async fn append_with_synchronizing_literal_returns_appenduid() {
    let message = "Subject: hi\r\n\r\nhello";
    let script = format!(
        r#"{}C: A0003 APPEND "Sent" (\Seen) {{{len}}}
        S: + Ready for literal data
        L: {escaped}
        S: A0003 OK [APPENDUID 38505 3955] APPEND completed
        C: A0004 APPEND "Missing" {{{len}}}
        S: A0004 NO [TRYCREATE] Mailbox doesn't exist
        C: A0005 NOOP
        S: A0005 OK
        "#,
        prelude("UIDPLUS"),
        len = message.len(),
        escaped = message.replace("\r\n", "\\r\\n"),
    );
    let (mut client, server) = connected(&script).await;
    let uid = client
        .append("Sent", message.as_bytes(), Flags::SEEN, &[])
        .await
        .unwrap();
    assert_eq!(
        uid,
        Some(AppendUid {
            uidvalidity: 38_505,
            uid: 3955
        })
    );
    match client
        .append("Missing", message.as_bytes(), Flags::empty(), &[])
        .await
        .unwrap_err()
    {
        ImapError::No { code, .. } => assert_eq!(code.as_deref(), Some("TRYCREATE")),
        other => panic!("unexpected {other:?}"),
    }
    // The session is still in sync after the rejected APPEND.
    client.noop().await.unwrap();
    server.done().await;
}

#[tokio::test]
async fn append_with_literal_plus_does_not_wait() {
    let script = format!(
        r#"{}C: A0003 APPEND "Drafts" (\Draft projekt) {{5+}}
        L: draft
        S: A0003 OK APPEND completed
        "#,
        prelude("LITERAL+")
    );
    let (mut client, server) = connected(&script).await;
    let uid = client
        // Keywords go into the flag list; one the server can't take is left out.
        .append(
            "Drafts",
            b"draft",
            Flags::DRAFT,
            &["projekt".to_owned(), "bad(".to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(uid, None);
    server.done().await;
}

#[tokio::test]
async fn folder_management_and_status() {
    let script = format!(
        r#"{}C: A0003 CREATE "Projekte/&AMQ-"
        S: A0003 OK done
        C: A0004 RENAME "Old" "New"
        S: A0004 OK done
        C: A0005 DELETE "New"
        S: A0005 OK done
        C: A0006 SUBSCRIBE "Projekte/&AMQ-"
        S: A0006 OK done
        C: A0007 UNSUBSCRIBE "Projekte/&AMQ-"
        S: A0007 OK done
        C: A0008 STATUS "Tom &- Jerry" (MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)
        S: * STATUS "Tom &- Jerry" (MESSAGES 231 UIDNEXT 44292 UIDVALIDITY 1 UNSEEN 3 HIGHESTMODSEQ 7011231777)
        S: A0008 OK STATUS completed
        C: A0009 CREATE "x\"y\\z"
        S: A0009 OK done
        "#,
        prelude("CONDSTORE")
    );
    let (mut client, server) = connected(&script).await;
    client.create("Projekte/Ä").await.unwrap();
    client.rename("Old", "New").await.unwrap();
    client.delete("New").await.unwrap();
    client.subscribe("Projekte/Ä", true).await.unwrap();
    client.subscribe("Projekte/Ä", false).await.unwrap();
    let status = client.status("Tom & Jerry").await.unwrap();
    assert_eq!(
        status,
        FolderStatus {
            messages: Some(231),
            unseen: Some(3),
            uidnext: Some(44_292),
            uidvalidity: Some(1),
            highest_modseq: Some(7_011_231_777),
        }
    );
    client.create("x\"y\\z").await.unwrap();
    assert!(matches!(
        client.create("bad\r\nname").await,
        Err(ImapError::InvalidInput(_))
    ));
    server.done().await;
}

#[tokio::test]
async fn uid_search_all_and_unsolicited_events() {
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 UID SEARCH ALL
        S: * SEARCH 1 2 3 7 8 9
        S: A0004 OK done
        C: A0005 NOOP
        S: * 4 EXISTS
        S: * 1 RECENT
        S: * 2 EXPUNGE
        S: * 1 FETCH (FLAGS (\Seen))
        S: * OK [ALERT] Maintenance at midnight
        S: A0005 OK NOOP completed
        "#,
        prelude("IDLE")
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    assert_eq!(
        client.uid_search_all().await.unwrap().to_string(),
        "1:3,7:9"
    );
    client.noop().await.unwrap();
    let events = client.take_events();
    assert_eq!(
        events,
        [
            SessionEvent::Exists(4),
            SessionEvent::Recent(1),
            SessionEvent::Expunge(2),
            SessionEvent::Flags {
                seq: 1,
                uid: None,
                flags: Flags::SEEN,
                keywords: vec![],
                modseq: None
            },
            SessionEvent::Alert("Maintenance at midnight".into()),
        ]
    );
    assert!(client.take_events().is_empty());
    server.done().await;
}

#[tokio::test]
async fn idle_reports_new_data() {
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 IDLE
        S: + idling
        S: * OK Still here
        S: * 4 EXISTS
        C: DONE
        S: * 1 RECENT
        S: A0004 OK IDLE terminated
        "#,
        prelude("IDLE")
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    let outcome = client
        .idle(CancellationToken::new(), Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(outcome, IdleOutcome::NewData);
    assert_eq!(
        client.take_events(),
        [SessionEvent::Exists(4), SessionEvent::Recent(1)]
    );
    server.done().await;
}

#[tokio::test]
async fn idle_times_out_and_cancels() {
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 IDLE
        S: + idling
        C: DONE
        S: A0004 OK IDLE terminated
        C: A0005 IDLE
        S: + idling
        C: DONE
        S: A0005 OK IDLE terminated
        "#,
        prelude("IDLE")
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    let outcome = client
        .idle(CancellationToken::new(), Duration::from_millis(50))
        .await
        .unwrap();
    assert_eq!(outcome, IdleOutcome::Timeout);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let outcome = client.idle(cancel, Duration::from_secs(30)).await.unwrap();
    assert_eq!(outcome, IdleOutcome::Cancelled);
    // A cancelled token returns immediately without a round trip.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        client
            .idle(cancelled, Duration::from_secs(1))
            .await
            .unwrap(),
        IdleOutcome::Cancelled
    );
    server.done().await;
}

#[tokio::test]
async fn idle_requires_capability_and_selection() {
    let script = prelude("MOVE");
    let (mut client, server) = connected(&script).await;
    assert!(matches!(
        client
            .idle(CancellationToken::new(), Duration::from_secs(1))
            .await,
        Err(ImapError::NoFolderSelected)
    ));
    server.done().await;
}

#[tokio::test]
async fn command_timeout_poisons_the_session() {
    let script = format!(
        "{}C: A0003 NOOP
         C: A0004 NOOP",
        prelude("IDLE")
    );
    let mut config = config();
    config.command_timeout = Duration::from_millis(100);
    let (client, server) = start(&script, &config).await;
    let mut client = client.unwrap();
    assert!(matches!(
        client.noop().await,
        Err(ImapError::Timeout {
            operation: "NOOP",
            ..
        })
    ));
    assert!(client.is_broken());
    assert!(matches!(client.noop().await, Err(ImapError::Broken)));
    drop(client);
    assert!(server.server.await.unwrap().is_err());
}

#[tokio::test]
async fn bye_during_command_is_connection_lost() {
    let script = format!(
        "{}C: A0003 NOOP
         S: * BYE Server shutting down",
        prelude("IDLE")
    );
    let (mut client, server) = connected(&script).await;
    match client.noop().await.unwrap_err() {
        ImapError::ConnectionLost(Some(text)) => assert_eq!(text, "Server shutting down"),
        other => panic!("unexpected {other:?}"),
    }
    assert!(client.is_broken());
    server.done().await;
}

#[tokio::test]
async fn unparseable_response_is_protocol_error_without_content() {
    // ESEARCH is not understood by imap-proto, which is why the client never asks
    // for it. If a server sends one anyway, the error must not echo the payload.
    let script = format!(
        r#"{}{SELECTED_INBOX}C: A0004 UID SEARCH ALL
        S: * ESEARCH (TAG "A0004") UID ALL 1:5 secret-subject
        S: A0004 OK done
        "#,
        prelude("IDLE")
    );
    let (mut client, server) = connected(&script).await;
    client.select("INBOX", None).await.unwrap();
    let err = client.uid_search_all().await.unwrap_err();
    assert!(matches!(err, ImapError::Protocol(_)), "{err:?}");
    assert!(!err.to_string().contains("secret-subject"));
    assert!(client.is_broken());
    server.done().await;
}

#[tokio::test]
async fn implicit_tls_with_custom_root() {
    let tls = test_tls();
    let script = format!(
        "{}C: A0003 NOOP
         S: A0003 OK",
        prelude("IDLE")
    );
    let config = config()
        .with_security(Security::Tls)
        .with_tls_verification(TlsVerification::CustomRoots(vec![tls.root.clone()]));
    let (client, server) = start_with_tls(&script, &config, Some(tls.server), true).await;
    let mut client = client.unwrap();
    client.noop().await.unwrap();
    server.done().await;
}

#[tokio::test]
async fn tls_rejects_untrusted_certificate() {
    let tls = test_tls();
    let other = test_tls();
    let config = config()
        .with_security(Security::Tls)
        .with_tls_verification(TlsVerification::CustomRoots(vec![other.root]));
    let (client, server) = start_with_tls("", &config, Some(tls.server), true).await;
    assert!(matches!(client.unwrap_err(), ImapError::Tls(_)));
    // The server side sees the aborted handshake.
    assert!(server.server.await.unwrap().is_err());
}

#[tokio::test]
async fn starttls_upgrades_before_login() {
    let tls = test_tls();
    let script = r#"
        S: * OK [CAPABILITY IMAP4rev1 STARTTLS LOGINDISABLED] ready
        C: A0001 STARTTLS
        S: A0001 OK Begin TLS negotiation now
        TLS
        C: A0001 CAPABILITY
        S: * CAPABILITY IMAP4rev1 AUTH=PLAIN
        S: A0001 OK done
        C: A0002 AUTHENTICATE PLAIN
        S: +
        C: AGFsaWNlAHNlY3JldA==
        S: A0002 OK authenticated
        C: A0003 CAPABILITY
        S: * CAPABILITY IMAP4rev1 IDLE
        S: A0003 OK done
    "#;
    let config = config()
        .with_security(Security::StartTls)
        .with_tls_verification(TlsVerification::CustomRoots(vec![tls.root.clone()]));
    let (client, server) = start_with_tls(script, &config, Some(tls.server), false).await;
    let client = client.unwrap();
    assert!(client.caps().idle());
    server.done().await;
}

#[tokio::test]
async fn starttls_is_required_when_configured() {
    let script = "S: * OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] ready";
    let config = config().with_security(Security::StartTls);
    let (client, server) = start(script, &config).await;
    assert!(matches!(
        client.unwrap_err(),
        ImapError::Unsupported("STARTTLS")
    ));
    server.done().await;
}

#[test]
fn keyword_and_section_validation() {
    assert!(validate_keyword("$label1").is_ok());
    assert!(validate_keyword("Work").is_ok());
    assert!(validate_keyword("\\Seen").is_err());
    assert!(validate_keyword("two words").is_err());
    assert!(validate_keyword("a(b").is_err());
    assert!(validate_keyword("").is_err());
    assert!(validate_section("1.2.MIME").is_ok());
    assert!(validate_section("HEADER").is_ok());
    assert!(validate_section("1]").is_err());
}

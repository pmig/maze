//! `LettreMailer` against a tiny in-process SMTP server: the login mechanisms, the
//! envelope, the bytes on the wire and how server replies are classified.

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mazemail_types::Sensitive;
use pretty_assertions::assert_eq;
use smtp::{
    Credentials, LettreMailer, Mailer as _, OutboxEnvelope, SEND_AS_DENIED_EXPLANATION, Security,
    SmtpConfig, SmtpError,
};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;

/// What the fake server saw.
#[derive(Debug, Default, Clone)]
struct Log {
    ehlo: Vec<String>,
    /// Decoded SASL responses, one per login.
    auth: Vec<(String, Vec<u8>)>,
    mail_from: Vec<String>,
    rcpt_to: Vec<String>,
    data: Vec<Vec<u8>>,
}

/// How the fake server answers.
#[derive(Debug, Clone, Copy)]
struct Replies {
    /// The `AUTH` mechanisms offered after `EHLO`.
    offer: &'static str,
    auth: &'static str,
    data: &'static str,
}

impl Default for Replies {
    fn default() -> Self {
        Self {
            offer: "PLAIN LOGIN XOAUTH2",
            auth: "235 2.7.0 Authentication successful",
            data: "250 2.0.0 OK queued",
        }
    }
}

struct Fake {
    port: u16,
    log: Arc<Mutex<Log>>,
}

impl Fake {
    async fn start(replies: Replies) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Log::default()));
        let shared = log.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = shared.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, replies, log).await;
                });
            }
        });
        Self { port, log }
    }

    fn config(&self) -> SmtpConfig {
        SmtpConfig::new("127.0.0.1", Security::PlainInsecure).with_port(self.port)
    }

    fn log(&self) -> Log {
        self.log.lock().unwrap().clone()
    }
}

fn decode(b64: &str) -> Vec<u8> {
    STANDARD.decode(b64.trim()).unwrap()
}

async fn serve(
    stream: tokio::net::TcpStream,
    replies: Replies,
    log: Arc<Mutex<Log>>,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read);
    write.write_all(b"220 fake.example ESMTP\r\n").await?;
    let mut line = String::new();
    loop {
        line.clear();
        if lines.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let command = line.trim_end().to_owned();
        let upper = command.to_ascii_uppercase();
        let reply: String = if upper.starts_with("EHLO") {
            log.lock().unwrap().ehlo.push(command.clone());
            format!(
                "250-fake.example\r\n250-AUTH {}\r\n250-8BITMIME\r\n250 SMTPUTF8",
                replies.offer
            )
        } else if let Some(rest) = upper.strip_prefix("AUTH LOGIN") {
            let _ = rest;
            write.write_all(b"334 VXNlcm5hbWU6\r\n").await?;
            line.clear();
            lines.read_line(&mut line).await?;
            let user = decode(&line);
            write.write_all(b"334 UGFzc3dvcmQ6\r\n").await?;
            line.clear();
            lines.read_line(&mut line).await?;
            let mut both = user;
            both.push(0);
            both.extend(decode(&line));
            log.lock().unwrap().auth.push(("LOGIN".into(), both));
            replies.auth.into()
        } else if upper.starts_with("AUTH ") {
            let mut parts = command.splitn(3, ' ');
            let (_, mechanism, response) = (parts.next(), parts.next(), parts.next());
            log.lock().unwrap().auth.push((
                mechanism.unwrap_or_default().to_ascii_uppercase(),
                decode(response.unwrap_or_default()),
            ));
            replies.auth.into()
        } else if upper.starts_with("MAIL FROM:") {
            log.lock().unwrap().mail_from.push(command[10..].to_owned());
            "250 2.1.0 OK".into()
        } else if upper.starts_with("RCPT TO:") {
            log.lock().unwrap().rcpt_to.push(command[8..].to_owned());
            "250 2.1.5 OK".into()
        } else if upper == "DATA" {
            write.write_all(b"354 Go ahead\r\n").await?;
            let mut data = Vec::new();
            loop {
                let mut raw = Vec::new();
                if lines.read_until(b'\n', &mut raw).await? == 0 {
                    return Ok(());
                }
                if raw == b".\r\n" {
                    break;
                }
                // Undo dot-stuffing.
                let raw = if raw.starts_with(b"..") {
                    &raw[1..]
                } else {
                    &raw[..]
                };
                data.extend_from_slice(raw);
            }
            // The CRLF before the final dot belongs to the terminator (RFC 5321 4.1.1.4).
            if data.ends_with(b"\r\n") {
                data.truncate(data.len() - 2);
            }
            log.lock().unwrap().data.push(data);
            replies.data.into()
        } else if upper == "QUIT" {
            write.write_all(b"221 2.0.0 Bye\r\n").await?;
            return Ok(());
        } else if upper == "NOOP" || upper == "RSET" {
            "250 2.0.0 OK".into()
        } else {
            "502 5.5.2 Unknown command".into()
        };
        write.write_all(reply.as_bytes()).await?;
        write.write_all(b"\r\n").await?;
    }
}

fn envelope() -> OutboxEnvelope {
    OutboxEnvelope {
        from: "alice@example.org".into(),
        to: vec!["bob@example.org".into(), "hidden@example.net".into()],
    }
}

const RAW: &[u8] = b"From: Alice <alice@example.org>\r\nTo: Bob <bob@example.org>\r\n\
Subject: Hello\r\nMessage-ID: <one@example.org>\r\n\r\nHi Bob,\r\n.leading dot\r\n\
Gr\xc3\xbc\xc3\x9fe\r\n";

fn password() -> Credentials {
    Credentials::Password {
        user: "alice".into(),
        password: Sensitive::new("secret".into()),
    }
}

#[tokio::test]
async fn plain_login_and_the_envelope_with_bcc() {
    let fake = Fake::start(Replies::default()).await;
    LettreMailer
        .send(&fake.config(), &password(), &envelope(), RAW)
        .await
        .unwrap();
    let log = fake.log();
    assert_eq!(log.ehlo, vec!["EHLO [127.0.0.1]".to_owned()]);
    assert_eq!(
        log.auth,
        vec![("PLAIN".to_owned(), b"\0alice\0secret".to_vec())]
    );
    assert!(log.mail_from[0].starts_with("<alice@example.org>"));
    // The Bcc recipient is only in the envelope; the message bytes arrive unchanged.
    assert_eq!(
        log.rcpt_to,
        vec![
            "<bob@example.org>".to_owned(),
            "<hidden@example.net>".to_owned()
        ]
    );
    assert_eq!(log.data, vec![RAW.to_vec()]);
}

#[tokio::test]
async fn login_when_the_server_offers_no_plain() {
    let fake = Fake::start(Replies {
        offer: "LOGIN",
        ..Replies::default()
    })
    .await;
    LettreMailer
        .send(&fake.config(), &password(), &envelope(), RAW)
        .await
        .unwrap();
    assert_eq!(
        fake.log().auth,
        vec![("LOGIN".to_owned(), b"alice\0secret".to_vec())]
    );
}

#[tokio::test]
async fn xoauth2_sends_the_string_auth_builds() {
    let fake = Fake::start(Replies::default()).await;
    let credentials = Credentials::XOAuth2 {
        user: "carol@gmail.com".into(),
        access_token: Sensitive::new("ya29.token".into()),
    };
    LettreMailer
        .send(&fake.config(), &credentials, &envelope(), RAW)
        .await
        .unwrap();
    let expected = auth::sasl::xoauth2("carol@gmail.com", "ya29.token").unwrap();
    assert_eq!(
        fake.log().auth,
        vec![("XOAUTH2".to_owned(), expected.expose().as_bytes().to_vec())]
    );

    // A token that would break the SASL framing never reaches the server.
    let bad = Credentials::XOAuth2 {
        user: "carol@gmail.com".into(),
        access_token: Sensitive::new("bad\u{1}token".into()),
    };
    let err = LettreMailer
        .send(&fake.config(), &bad, &envelope(), RAW)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SmtpError::Credentials {
                needs_user: true,
                ..
            }
        ),
        "{err:?}"
    );
    assert_eq!(fake.log().auth.len(), 1);
}

#[tokio::test]
async fn send_as_denied_is_a_typed_permanent_error() {
    let fake = Fake::start(Replies {
        data: "554 5.2.252 SendAsDenied; carol@contoso.example not allowed to send as \
               you@distr.example; STOREDRV.Submission.Exception:SendAsDeniedException",
        ..Replies::default()
    })
    .await;
    let err = LettreMailer
        .send(&fake.config(), &password(), &envelope(), RAW)
        .await
        .unwrap_err();
    let SmtpError::SendAsDenied { reply } = &err else {
        panic!("not SendAsDenied: {err:?}");
    };
    assert!(reply.contains("5.2.252"), "{reply}");
    assert!(err.is_permanent());
    assert_eq!(err.fix_it(), Some(SEND_AS_DENIED_EXPLANATION));
}

#[tokio::test]
async fn a_refused_login_is_an_auth_failure() {
    let fake = Fake::start(Replies {
        auth: "535 5.7.8 Username and Password not accepted",
        ..Replies::default()
    })
    .await;
    let err = LettreMailer
        .send(&fake.config(), &password(), &envelope(), RAW)
        .await
        .unwrap_err();
    assert!(matches!(err, SmtpError::AuthFailed { .. }), "{err:?}");
    assert!(err.is_auth() && err.is_permanent());
    assert!(fake.log().data.is_empty());

    let err = LettreMailer
        .check(&fake.config(), &password())
        .await
        .unwrap_err();
    assert!(err.is_auth(), "{err:?}");
}

#[tokio::test]
async fn check_logs_in_without_sending() {
    let fake = Fake::start(Replies::default()).await;
    LettreMailer
        .check(&fake.config(), &password())
        .await
        .unwrap();
    let log = fake.log();
    assert_eq!(log.auth.len(), 1);
    assert!(log.mail_from.is_empty() && log.data.is_empty());
}

#[tokio::test]
async fn temporary_rejections_and_unreachable_servers_are_retried() {
    let fake = Fake::start(Replies {
        data: "451 4.3.0 Try again later",
        ..Replies::default()
    })
    .await;
    let err = LettreMailer
        .send(&fake.config(), &password(), &envelope(), RAW)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            SmtpError::Rejected {
                code: 451,
                permanent: false,
                ..
            }
        ),
        "{err:?}"
    );

    // Nothing listens on this port any more.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let config = SmtpConfig::new("127.0.0.1", Security::PlainInsecure).with_port(port);
    let err = LettreMailer
        .send(&config, &password(), &envelope(), RAW)
        .await
        .unwrap_err();
    assert!(matches!(err, SmtpError::Connection { .. }), "{err:?}");
    assert!(!err.is_permanent());
}

#[tokio::test]
async fn bad_envelope_addresses_never_reach_the_server() {
    let fake = Fake::start(Replies::default()).await;
    let mut bad = envelope();
    bad.to.push("not an address".into());
    let err = LettreMailer
        .send(&fake.config(), &password(), &bad, RAW)
        .await
        .unwrap_err();
    assert!(matches!(err, SmtpError::InvalidAddress { .. }), "{err:?}");
    assert!(!err.to_string().contains("not an address"));
    assert!(fake.log().ehlo.is_empty());
}

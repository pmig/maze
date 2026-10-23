//! `LettreMailer` against a real Stalwart server: submit with a password login, then
//! find the message in the recipient's INBOX over IMAP.
//!
//! Ignored by default because they download and run a server binary. Run them with
//!
//! ```sh
//! MAZEMAIL_TEST_IMAP=1 cargo nextest run -p smtp --run-ignored only
//! ```

#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

mod support;

use std::time::{Duration, Instant};

use imap::{ImapAuth, ImapClient, ImapConfig, ImapSession as _, Security as ImapSecurity, UidSet};
use mazemail_types::Sensitive;
use smtp::{
    Credentials, LettreMailer, Mailer as _, OutboxEnvelope, Security, SmtpConfig, SmtpError,
};
use support::Stalwart;

#[allow(clippy::print_stderr)] // Tell the developer why the test did nothing.
fn server() -> Option<Stalwart> {
    if !support::enabled() {
        eprintln!("skipping: MAZEMAIL_TEST_IMAP is not 1");
        return None;
    }
    Some(Stalwart::start().expect("start stalwart"))
}

fn credentials(password: &str) -> Credentials {
    Credentials::Password {
        user: support::USER.into(),
        password: Sensitive::new(password.into()),
    }
}

#[tokio::test]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn submit_and_receive() {
    let Some(server) = server() else { return };
    let config = SmtpConfig::new("127.0.0.1", Security::PlainInsecure).with_port(server.smtp_port);

    LettreMailer
        .check(&config, &credentials(support::PASSWORD))
        .await
        .unwrap_or_else(|err| panic!("{err}\n{}", server.log()));
    let err = LettreMailer
        .check(&config, &credentials("wrong"))
        .await
        .unwrap_err();
    assert!(matches!(err, SmtpError::AuthFailed { .. }), "{err:?}");

    let raw = b"From: Alice <alice@example.org>\r\nTo: Alice <alice@example.org>\r\n\
Subject: Submitted by Maze\r\nMessage-ID: <smtp-1@example.org>\r\n\
Date: Thu, 22 Oct 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\nHello from the outbox.\r\n";
    let envelope = OutboxEnvelope {
        from: "alice@example.org".into(),
        to: vec!["alice@example.org".into()],
    };
    LettreMailer
        .send(&config, &credentials(support::PASSWORD), &envelope, raw)
        .await
        .unwrap_or_else(|err| panic!("{err}\n{}", server.log()));

    let mut imap = ImapClient::connect(
        &ImapConfig::new(
            "127.0.0.1",
            server.port,
            ImapAuth::password(support::USER, support::PASSWORD),
        )
        .with_security(ImapSecurity::PlainInsecure),
    )
    .await
    .unwrap();
    // Delivery goes through Stalwart's queue; give it a moment.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let info = imap.select("INBOX", None).await.unwrap();
        if info.exists > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "not delivered\n{}", server.log());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let headers = imap.fetch_headers(&UidSet::all()).await.unwrap();
    assert!(headers.iter().any(|h| {
        h.header
            .expose()
            .windows(18)
            .any(|w| w == b"smtp-1@example.org")
    }));
    imap.logout().await.unwrap();
}

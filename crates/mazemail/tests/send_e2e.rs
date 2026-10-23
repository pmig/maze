//! Sending end to end against a real server: the core adds a Stalwart account (IMAP and
//! SMTP submission with the same password), sends a reply-less message to the account
//! itself through the outbox and `LettreMailer`, and the Sent copy is uploaded into the
//! server's Sent folder (Stalwart doesn't file one itself).
//!
//! Ignored by default because it downloads and runs a server binary:
//!
//! ```sh
//! MAZEMAIL_TEST_IMAP=1 cargo nextest run -p mazemail --run-ignored only --test send_e2e
//! ```

// Tests may unwrap and print (AGENTS.md).
#![allow(clippy::unwrap_used, clippy::print_stderr)]

#[path = "../../smtp/tests/support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use auth::MemorySecretStore;
use imap::{ImapAuth, ImapClient, ImapConfig, ImapSession as _, Security, UidSet};
use mazemail_core::{
    AddImapAccount, CoreConfig, CoreHandle, Draft, ImapSecurity, ImapServer, SendResult,
    SmtpServerSettings, SyncEngineConfig, SyncState,
};
use mazemail_types::{Address, FolderRole, Sensitive};
use support::Stalwart;
use vault::MemoryKeyStore;

const SUBJECT: &str = "Sent by Maze end to end";

async fn imap(server: &Stalwart) -> ImapClient {
    ImapClient::connect(
        &ImapConfig::new(
            "127.0.0.1",
            server.port,
            ImapAuth::password(support::USER, support::PASSWORD),
        )
        .with_security(Security::PlainInsecure),
    )
    .await
    .unwrap()
}

/// Whether the server's folder with `role` has a message with [`SUBJECT`].
async fn has_message(server: &Stalwart, role: FolderRole) -> bool {
    let mut session = imap(server).await;
    let folders = session.list().await.unwrap();
    let Some(folder) = folders.iter().find(|f| f.role == Some(role)) else {
        return false;
    };
    let name = folder.name.clone();
    let info = session.select(&name, None).await.unwrap();
    let found = info.exists > 0
        && session
            .fetch_headers(&UidSet::all())
            .await
            .unwrap()
            .iter()
            .any(|h| {
                h.header
                    .expose()
                    .windows(SUBJECT.len())
                    .any(|w| w == SUBJECT.as_bytes())
            });
    let _ = session.logout().await;
    found
}

async fn wait_until(what: &str, server: &Stalwart, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !check().await {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}\n{}",
            server.log()
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Stalwart; set MAZEMAIL_TEST_IMAP=1 and run ignored tests"]
async fn send_with_a_sent_copy() {
    if !support::enabled() {
        eprintln!("skipping: MAZEMAIL_TEST_IMAP is not 1");
        return;
    }
    let server = Stalwart::start().expect("start stalwart");
    let dir = tempfile::tempdir().unwrap();
    let config = CoreConfig::new(dir.path(), Arc::new(MemoryKeyStore::new()))
        .with_sync_engine(SyncEngineConfig::new(Arc::new(MemorySecretStore::new())));
    let core = CoreHandle::open(config).await.unwrap();

    let account = core
        .add_imap_account(AddImapAccount {
            display_name: Some("Stalwart".into()),
            email: format!("{}@example.org", support::USER),
            server: ImapServer {
                host: "127.0.0.1".into(),
                port: server.port,
                security: ImapSecurity::Insecure,
            },
            username: Some(support::USER.into()),
            password: Sensitive::new(support::PASSWORD.into()),
            smtp: Some(SmtpServerSettings {
                host: "127.0.0.1".into(),
                port: server.smtp_port,
                security: ImapSecurity::Insecure,
            }),
        })
        .await
        .unwrap_or_else(|err| panic!("{err}\n{}", server.log()));
    // The folders (Sent among them) are known once the first sync is through.
    let deadline = Instant::now() + Duration::from_secs(30);
    while core
        .sync_status(account.id)
        .is_none_or(|s| s.state != SyncState::Idle)
    {
        assert!(Instant::now() < deadline, "no first sync\n{}", server.log());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let draft = Draft {
        to: vec![Address::new(Some("Alice"), "alice@example.org")],
        subject: SUBJECT.into(),
        body_markdown: "Hello from the **composer**.".into(),
        ..core.new_draft().unwrap()
    };
    let result = core
        .send_draft(None, draft)
        .await
        .unwrap_or_else(|err| panic!("{err}\n{}", server.log()));
    assert_eq!(result, SendResult::Sent);

    wait_until("the delivery", &server, async || {
        has_message(&server, FolderRole::Inbox).await
    })
    .await;
    wait_until("the Sent copy", &server, async || {
        has_message(&server, FolderRole::Sent).await
    })
    .await;
    core.shutdown().await.unwrap();
}

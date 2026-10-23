//! Agent sessions against a real core: the per-account AI policy in session tokens and
//! thread exports (through the socket server too), stored conversations, and the
//! proposal review.

use std::collections::BTreeSet;
use std::sync::Arc;

use local_api::{
    AccountPolicy as _, LocalApiClient, LocalApiServer, Profile, ServerConfig, TokenStore as _,
};
use mazemail_core::api::{self, SettingsAccountPolicy, StoreTokenStore, TokenAuthorizer};
use mazemail_core::{CoreHandle, MailCommand, ProposalItemState};
use mazemail_types::{MessageId, ThreadId};
use serde_json::json;
use vault::MemoryKeyStore;

use crate::common::{self, Fixture, Place};

struct AgentCore {
    core: CoreHandle,
    authorizer: TokenAuthorizer,
    fixture: Fixture,
    work_mail: MessageId,
    work_thread: ThreadId,
    gmail_mail: MessageId,
    gmail_thread: ThreadId,
    _dir: tempfile::TempDir,
}

/// Work shares mail with agents ("any"); Gmail is "off".
async fn open() -> AgentCore {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        r#"{ "agent": { "accounts": { "Work": "any", "Gmail": "off" } } }"#,
    )
    .unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let authorizer = TokenAuthorizer::new();
    let config = common::config(&dir.path().join("data"), &keys)
        .with_settings_file(&settings)
        .with_authorizer(Arc::new(authorizer.clone()));
    let core = CoreHandle::open(config).await.unwrap();
    let fixture = common::fixture(&core).await;
    let raw = |n: u32, from: &str, subject: &str| {
        common::raw_message(
            &format!("agent{n}@example.com"),
            from,
            "me@work.example",
            subject,
            &format!("Mon, {n} Oct 2025 10:00:00 +0000"),
            "Budget numbers for the offsite are attached.",
        )
    };
    let (work_mail, work_thread) = common::ingest(
        &core,
        Place::new(fixture.work, fixture.inbox, 1),
        raw(6, "Anna <anna@example.com>", "Offsite budget"),
    )
    .await;
    let (gmail_mail, gmail_thread) = common::ingest(
        &core,
        Place::new(fixture.gmail, fixture.g_inbox, 2),
        raw(7, "Bank <alerts@bank.example>", "Offsite payment"),
    )
    .await;
    core.commit_index().await.unwrap();
    AgentCore {
        core,
        authorizer,
        fixture,
        work_mail,
        work_thread,
        gmail_mail,
        gmail_thread,
        _dir: dir,
    }
}

fn token_store(t: &AgentCore) -> StoreTokenStore {
    StoreTokenStore::new(t.core.clone(), Arc::clone(t.authorizer.cache()))
}

#[tokio::test]
async fn an_off_account_is_never_in_an_agent_session() {
    let t = open().await;
    let policy = SettingsAccountPolicy::new(t.core.clone());
    assert_eq!(policy.agent_accounts(), BTreeSet::from([t.fixture.work]));

    // The session token's scope lists only the shared account.
    let store = token_store(&t);
    let minted =
        agent::tokens::mint_session_token(&store, &policy, Profile::Triage, "Claude Code", "s_1")
            .await
            .unwrap();
    assert_eq!(
        minted.record.scope.accounts,
        Some(BTreeSet::from([t.fixture.work]))
    );

    // The thread export for the scratch dir refuses the Gmail thread.
    let work = api::thread_markdown_for_agents(&t.core, t.work_thread)
        .await
        .unwrap()
        .unwrap();
    assert!(work.contains("Offsite budget"), "{work}");
    assert!(
        api::thread_markdown_for_agents(&t.core, t.gmail_thread)
            .await
            .unwrap()
            .is_none()
    );

    // Through the real socket with the session token: Gmail mail doesn't exist.
    let socket_dir = tempfile::Builder::new()
        .prefix("mz")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = settings::SocketPath::InDataDir(socket_dir.path().join("m.sock"));
    let services = api::services(&t.core, &t.authorizer, Arc::new(local_api::DenyAllApprover));
    let server = LocalApiServer::bind_socket(&socket, None, services, ServerConfig::default())
        .await
        .unwrap();
    let endpoint = local_api::Endpoint::Unix(server.path().to_owned());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut client = LocalApiClient::connect(&endpoint)
        .await
        .unwrap()
        .with_token(Some(minted.token.clone()));
    let hits = client
        .call("search", json!({ "query": "offsite" }))
        .await
        .unwrap();
    let ids: BTreeSet<String> = hits
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, BTreeSet::from([t.work_mail.to_string()]));
    let error = client
        .call(
            "show",
            json!({ "id": t.gmail_mail.to_string(), "format": "md" }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.exit_code().code(), 1, "{error}");

    // Revoked at the end of the session: no more mail.
    agent::tokens::revoke(&store, &minted.record.id).await;
    let mut client = LocalApiClient::connect(&endpoint)
        .await
        .unwrap()
        .with_token(Some(minted.token));
    let error = client
        .call("search", json!({ "query": "offsite" }))
        .await
        .unwrap_err();
    assert_eq!(error.exit_code().code(), 6, "{error}");
    let _ = stop.send(());
    serving.await.unwrap();
}

#[tokio::test]
async fn conversations_and_transcripts_are_stored() {
    let t = open().await;
    let id = t
        .core
        .create_agent_thread("claude-acp", None, Some(t.work_thread))
        .await
        .unwrap();
    t.core
        .update_agent_thread(id, Some("Summarize"), Some("acp-1"))
        .await
        .unwrap();
    t.core
        .save_agent_transcript(id, br#"{"entries":[]}"#.to_vec())
        .await
        .unwrap();
    let threads = t.core.agent_threads(Some(t.work_thread)).unwrap();
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].acp_session_id.as_deref(), Some("acp-1"));
    assert!(
        t.core
            .agent_threads(Some(t.gmail_thread))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        t.core.agent_transcript(id).await.unwrap().unwrap(),
        br#"{"entries":[]}"#
    );
    t.core.delete_agent_thread(id).await.unwrap();
    assert!(t.core.agent_threads(None).unwrap().is_empty());
}

#[tokio::test]
async fn proposals_are_reviewed_item_by_item() {
    let t = open().await;
    let store = token_store(&t);
    let policy = SettingsAccountPolicy::new(t.core.clone());
    let minted =
        agent::tokens::mint_session_token(&store, &policy, Profile::Triage, "Codex", "s_2")
            .await
            .unwrap();
    // The authorizer learns the token like on a request.
    store.find_by_hash(&minted.record.hash).await.unwrap();
    let token = t.authorizer.cache().store_id(&minted.record.id).unwrap();

    // Two triage commands become one proposal each.
    for command in [
        MailCommand::Archive {
            ids: vec![t.work_mail],
        },
        MailCommand::SetFlags {
            ids: vec![t.work_mail],
            add: mazemail_types::Flags::FLAGGED,
            remove: mazemail_types::Flags::empty(),
        },
    ] {
        let receipt = t
            .core
            .apply(command, mazemail_core::Origin::Agent { token })
            .await
            .unwrap();
        assert!(matches!(
            receipt.outcome,
            mazemail_core::CommandOutcome::Proposed(_)
        ));
    }
    let pending = t.core.pending_proposals().unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].source.as_deref(), Some("Codex"));
    let flag = &pending[0];
    let archive = &pending[1];
    assert_eq!(archive.items[0].summary, "Archive 1 message");

    // Reject the flag, accept the archive.
    t.core
        .decide_proposal_item(flag.items[0].id, false)
        .await
        .unwrap();
    t.core.decide_proposal(archive.id, true).await.unwrap();
    assert!(t.core.pending_proposals().unwrap().is_empty());
    let reader = t.core.store().reader().unwrap();
    use store::Queries as _;
    let stored = reader.proposal(archive.id).unwrap().unwrap();
    assert_eq!(stored.items[0].state, ProposalItemState::Applied);
    assert_eq!(stored.state, store::ProposalState::Applied);
    let rejected = reader.proposal(flag.id).unwrap().unwrap();
    assert_eq!(rejected.state, store::ProposalState::Rejected);
    // The archive really happened: the message left the inbox.
    let detail = reader.message(t.work_mail).unwrap().unwrap();
    assert!(detail.locations.iter().all(|l| l.folder != t.fixture.inbox));
}

//! `AgentManager::start` end to end with a real agent process: the probe agent
//! (`src/bin/mazemail-probe-agent.rs`) reports what it sees from inside the session.

#![cfg(feature = "test-support")]
#![allow(clippy::unwrap_used)] // Tests may unwrap (AGENTS.md).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent::http::tests::MemoryHttp;
use agent::{
    AgentManager, AgentPaths, Entry, ManagerConfig, MemoryHost, SessionEvent, SessionStatus,
    StartRequest, Transcript,
};
use local_api::TokenStore as _;
use mazemail_types::{AccountId, ThreadId};
use settings::{CustomAgentSettings, Settings};

const PROBE: &str = env!("CARGO_BIN_EXE_mazemail-probe-agent");

async fn until(
    events: &async_channel::Receiver<SessionEvent>,
    transcript: &mut Transcript,
    status: SessionStatus,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match events.recv().await.unwrap() {
                SessionEvent::Update(update) => transcript.apply(&update),
                SessionEvent::Status(s) if s == status => return,
                SessionEvent::Ended(error) => panic!("the session ended: {error:?}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_started_agent_sees_only_its_session() {
    let dir = tempfile::tempdir().unwrap();
    // A fake home directory with the data directory inside, like on macOS.
    let home = std::fs::canonicalize(dir.path()).unwrap();
    let data = home.join("Library/Application Support/mazemail");
    std::fs::create_dir_all(data.join("mail")).unwrap();
    std::fs::write(home.join("secret.txt"), "secret").unwrap();
    let host = Arc::new(MemoryHost::new([AccountId(1)]));
    host.add_thread_export(ThreadId(5), "# Offsite plan");
    let manager = AgentManager::new(ManagerConfig {
        paths: AgentPaths::new(&data),
        home: Some(home.clone()),
        cli_program: None,
        socket: None,
        host: Arc::clone(&host) as Arc<dyn agent::AgentHost>,
        http: Arc::new(MemoryHttp::default()),
    });
    let mut settings = Settings::default();
    settings.agent.custom.insert(
        "probe".to_owned(),
        CustomAgentSettings {
            name: Some("Probe".to_owned()),
            command: PROBE.to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
    );
    // The sandbox only exists on macOS in this test environment.
    settings.agent_sandbox.enabled = cfg!(target_os = "macos");

    let started = manager
        .start(StartRequest {
            agent_id: "probe".to_owned(),
            mail_thread: Some(ThreadId(5)),
            resume: None,
            settings,
        })
        .await
        .unwrap();
    assert_eq!(started.sandboxed, cfg!(target_os = "macos"));
    assert!(started.attached.is_some());
    let handle = started.handle;
    let scratch = data.join("sessions").join(handle.id());
    assert!(scratch.join("AGENTS.md").is_file());
    let events = handle.events();
    let mut transcript = Transcript::default();
    until(&events, &mut transcript, SessionStatus::Ready).await;

    handle.prompt(format!("probe {}", home.join("secret.txt").display()));
    until(&events, &mut transcript, SessionStatus::Working).await;
    until(&events, &mut transcript, SessionStatus::Ready).await;
    let report = transcript
        .entries
        .iter()
        .find_map(|e| match e {
            Entry::Agent { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    let field = |name: &str| -> String {
        report
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}=")))
            .unwrap_or_else(|| panic!("{name} missing in {report}"))
            .to_owned()
    };
    assert_eq!(PathBuf::from(field("home")), data.join("agent-home/probe"));
    assert_eq!(PathBuf::from(field("cwd")), scratch);
    assert_eq!(field("token"), "true");
    assert_eq!(PathBuf::from(field("data_dir")), data);
    assert_eq!(field("agents_md"), "true");
    assert_eq!(field("thread"), "# Offsite plan");
    assert_eq!(field("leaked"), "none", "Maze's environment must not leak");
    assert_eq!(field("tmp_writable"), "true");
    if cfg!(target_os = "macos") {
        assert_eq!(field("secret"), "denied", "the home directory is hidden");
    }

    // Ending the session revokes the token and deletes the folder.
    drop(handle);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let SessionEvent::Ended(error) = events.recv().await.unwrap() {
                assert_eq!(error, None);
                return;
            }
        }
    })
    .await
    .unwrap();
    assert!(!scratch.exists());
    let tokens = host.tokens().list().await.unwrap();
    assert_eq!(tokens.len(), 1);
    assert!(tokens[0].revoked_at.is_some());
}

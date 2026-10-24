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
    assert_eq!(field("open_urls"), "true");
    assert_eq!(field("browser"), "true");
    assert_eq!(field("anthropic_key_sha256"), "none");
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

fn sha256(text: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(text.as_bytes()))
}

/// Files under `dir` (recursively) that contain `needle`.
fn files_containing(dir: &std::path::Path, needle: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(files_containing(&path, needle));
        } else if std::fs::read(&path)
            .unwrap_or_default()
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
        {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn a_saved_key_reaches_the_agent_and_nothing_else() {
    const KEY: &str = "sk-ant-oat01-ProbeKeyCanary";
    let dir = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    let data = home.join("data");
    let settings_file = home.join("settings.json");
    std::fs::write(&settings_file, "{}").unwrap();
    let host = Arc::new(MemoryHost::new([AccountId(1)]));
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
    settings.agent_sandbox.enabled = cfg!(target_os = "macos");

    // A custom agent takes a key when its sign-in method names a provider.
    assert!(manager.save_key("probe", None, KEY).await.is_err());
    let env = manager
        .save_key("probe", Some("anthropic"), &format!("  {KEY}\n"))
        .await
        .unwrap();
    assert_eq!(env, "CLAUDE_CODE_OAUTH_TOKEN");
    assert_eq!(
        manager.key_envs().await.unwrap(),
        BTreeMap::from([("probe".to_owned(), "CLAUDE_CODE_OAUTH_TOKEN".to_owned())])
    );
    assert_eq!(host.key("probe").unwrap().key.expose(), KEY);

    let started = manager
        .start(StartRequest {
            agent_id: "probe".to_owned(),
            mail_thread: None,
            resume: None,
            settings: settings.clone(),
        })
        .await
        .unwrap();
    assert_eq!(started.key_env.as_deref(), Some("CLAUDE_CODE_OAUTH_TOKEN"));
    let handle = started.handle;
    let events = handle.events();
    let mut transcript = Transcript::default();
    until(&events, &mut transcript, SessionStatus::Ready).await;
    handle.prompt("probe /nonexistent");
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
    assert!(report.contains(&format!("oauth_token_sha256={}", sha256(KEY))));
    assert!(report.contains("anthropic_key_sha256=none"));

    // Never in plaintext on disk (settings, agent home, scratch), nor in the transcript.
    assert!(
        !transcript
            .to_json()
            .windows(KEY.len())
            .any(|w| w == KEY.as_bytes())
    );
    assert_eq!(files_containing(&home, KEY), Vec::<PathBuf>::new());

    // Removing the key: the next session starts without it.
    manager.remove_key("probe").await.unwrap();
    assert!(host.key("probe").is_none());
    assert!(manager.key_envs().await.unwrap().is_empty());
    drop(handle);
    let started = manager
        .start(StartRequest {
            agent_id: "probe".to_owned(),
            mail_thread: None,
            resume: None,
            settings,
        })
        .await
        .unwrap();
    assert_eq!(started.key_env, None);
}

#[tokio::test]
async fn terminal_logins_become_a_script_without_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    let data = home.join("data");
    let host = Arc::new(MemoryHost::new([AccountId(1)]));
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
            args: vec!["--acp".to_owned()],
            env: BTreeMap::from([("PROBE_MODE".to_owned(), "1".to_owned())]),
        },
    );
    settings.agent_sandbox.enabled = cfg!(target_os = "macos");
    manager
        .save_key("probe", Some("anthropic"), "sk-ant-api03-LoginScriptCanary")
        .await
        .unwrap();
    let method = agent::AuthMethodInfo {
        id: "claude-ai-login".to_owned(),
        name: "Claude Subscription".to_owned(),
        description: None,
        terminal: Some(agent::TerminalAuth {
            command: None,
            args: vec!["--cli".to_owned(), "auth".to_owned(), "login".to_owned()],
            env: BTreeMap::from([("LOGIN_FLAG".to_owned(), "yes".to_owned())]),
        }),
        api_key_provider: None,
    };
    let login = manager.terminal_login("probe", &method, &settings).unwrap();
    let ext = if cfg!(target_os = "macos") {
        "command"
    } else {
        "sh"
    };
    assert_eq!(login.path, data.join(format!("agents/login/probe.{ext}")));
    assert_eq!(login.sandboxed, cfg!(target_os = "macos"));
    assert_eq!(login.display, format!("{PROBE} --acp --cli auth login"));
    let script = std::fs::read_to_string(&login.path).unwrap();
    let agent_home = data.join("agent-home/probe");
    assert!(script.contains(&format!("'HOME={}'", agent_home.display())));
    assert!(script.contains("'PROBE_MODE=1'"));
    assert!(script.contains("'LOGIN_FLAG=yes'"));
    assert!(script.contains(&format!("'{PROBE}' '--acp' '--cli' 'auth' 'login'")));
    assert!(script.contains(&format!(
        "'MAZEMAIL_OPEN_URLS={}'",
        agent_home.join(".mazemail-open-urls").display()
    )));
    // Neither a session token nor the saved key.
    assert!(!script.contains("mazemail_s_"));
    assert!(!script.contains("LoginScriptCanary"));
    if cfg!(target_os = "macos") {
        assert!(script.contains("'/usr/bin/sandbox-exec' '-p'"));
        assert!(script.contains(&format!(
            "(deny file-read* (subpath \"{}\"))",
            home.display()
        )));
    }
    // Not a terminal method.
    let mut plain = method;
    plain.terminal = None;
    assert!(manager.terminal_login("probe", &plain, &settings).is_err());
}

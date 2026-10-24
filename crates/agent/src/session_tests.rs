//! Sessions against the scripted in-process agent ([`crate::fake`]).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use local_api::{Profile, TokenStore as _};
use mazemail_types::{AccountId, ThreadId};
use pretty_assertions::assert_eq;

use crate::fake::{FakeAgent, FakeStep};
use crate::host::{AgentHost as _, MemoryHost};
use crate::paths::AgentPaths;
use crate::scratch::Scratch;
use crate::session::{
    AttachedThread, SessionConfig, SessionEvent, SessionHandle, SessionResources, SessionStatus,
    start_with_transport,
};
use crate::transcript::{
    Entry, EntryUpdate, PermissionChoice, PermissionKind, TerminalStatus, ToolStatus, Transcript,
};

const TIMEOUT: Duration = Duration::from_secs(20);

struct Harness {
    _dir: tempfile::TempDir,
    dir: PathBuf,
    host: Arc<MemoryHost>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        Self {
            _dir: dir,
            dir: path,
            host: Arc::new(MemoryHost::new([AccountId(1)])),
        }
    }

    /// A fake `mazemail` CLI that echoes its arguments.
    fn cli(&self) -> PathBuf {
        let cli = self.dir.join("bin/mazemail");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "#!/bin/sh\necho \"cli:$*\"\n").unwrap();
        crate::archive::make_executable(&cli).unwrap();
        cli
    }

    async fn config(&self) -> SessionConfig {
        let key = self
            .host
            .create_agent_thread("fake", None, Some(ThreadId(7)))
            .await
            .unwrap();
        SessionConfig {
            session_id: "s_test".to_owned(),
            agent_id: "fake".to_owned(),
            agent_name: "Fake".to_owned(),
            agent_thread: key,
            resume_acp_session: None,
            cwd: self.dir.clone(),
            terminal_env: BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]),
            cli_program: Some(self.cli()),
            terminal_policy: None,
            attached: None,
            transcript: Transcript::default(),
            open_urls: None,
            key_env: None,
            preferred: BTreeMap::new(),
        }
    }

    fn start(
        &self,
        agent: &FakeAgent,
        config: SessionConfig,
        resources: SessionResources,
    ) -> SessionHandle {
        start_with_transport(
            agent.transport(),
            config,
            Arc::clone(&self.host) as Arc<dyn crate::AgentHost>,
            resources,
            None,
        )
    }
}

/// Events until `stop` matches (inclusive); the transcript they build.
async fn until(
    handle: &SessionHandle,
    transcript: &mut Transcript,
    mut stop: impl FnMut(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    let events = handle.events();
    let mut seen = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let event = events.recv().await.expect("the session ended early");
            if let SessionEvent::Update(update) = &event {
                transcript.apply(update);
            }
            let done = stop(&event);
            seen.push(event);
            if done {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; events so far: {seen:#?}"));
    seen
}

fn is_status(status: SessionStatus) -> impl FnMut(&SessionEvent) -> bool {
    move |event| *event == SessionEvent::Status(status)
}

/// The next pending permission prompt.
fn permission_request(event: &SessionEvent) -> Option<(u64, PermissionKind)> {
    match event {
        SessionEvent::Update(EntryUpdate::Permission(prompt)) => Some((prompt.id, prompt.kind)),
        _ => None,
    }
}

#[tokio::test]
async fn a_turn_streams_into_the_transcript_and_is_stored() {
    let h = Harness::new();
    let agent = FakeAgent::new(vec![
        FakeStep::Thought("Reading the thread".into()),
        FakeStep::ToolCall {
            id: "t1".into(),
            title: "Read thread.md".into(),
        },
        FakeStep::ToolDone { id: "t1".into() },
        FakeStep::Plan(vec!["Summarize".into(), "Draft".into()]),
        FakeStep::Message("Anna wants ".into()),
        FakeStep::Message("the **offsite** on Friday.".into()),
    ]);
    let mut config = h.config().await;
    config.attached = Some(AttachedThread {
        public_id: "t_h".into(),
    });
    let key = config.agent_thread;
    let handle = h.start(&agent, config, SessionResources::default());
    let mut transcript = Transcript::default();
    let events = until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    assert!(events.contains(&SessionEvent::AgentInfo {
        name: "fake-agent".into(),
        version: "1.0.0".into()
    }));

    handle.prompt("Summarize this");
    until(&handle, &mut transcript, is_status(SessionStatus::Working)).await;
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;

    assert_eq!(
        transcript.entries,
        vec![
            Entry::User {
                text: "Summarize this".into()
            },
            Entry::Thought {
                text: "Reading the thread".into()
            },
            Entry::ToolCall(crate::transcript::ToolCallEntry {
                id: "t1".into(),
                title: "Read thread.md".into(),
                kind: "read".into(),
                status: ToolStatus::Completed,
                detail: String::new(),
            }),
            Entry::Plan {
                items: vec![
                    crate::transcript::PlanItem {
                        content: "Summarize".into(),
                        status: crate::transcript::PlanStatus::Pending
                    },
                    crate::transcript::PlanItem {
                        content: "Draft".into(),
                        status: crate::transcript::PlanStatus::Pending
                    },
                ]
            },
            Entry::Agent {
                text: "Anna wants the **offsite** on Friday.".into()
            },
        ]
    );

    let log = agent.log();
    assert_eq!(
        log[0],
        "initialize:terminal=true:fs_read=false:auth_terminal=true:meta_terminal_auth=true"
    );
    assert_eq!(log[1], format!("new_session:{}", h.dir.display()));
    // The attached thread is announced once, before the user's text.
    assert!(
        log[2].starts_with("prompt:Context: the mail thread t_h"),
        "{}",
        log[2]
    );
    assert!(log[2].ends_with(" | Summarize this"));

    // The transcript and the ACP session id were stored after the turn.
    let stored = Transcript::from_json(&h.host.transcript(key).unwrap());
    assert_eq!(stored, transcript);
    let threads = h.host.agent_threads(Some(ThreadId(7))).await.unwrap();
    assert_eq!(threads[0].acp_session_id.as_deref(), Some("fake-session-1"));
    assert_eq!(threads[0].title.as_deref(), Some("Summarize this"));
}

#[tokio::test]
async fn permission_prompts_offer_three_choices() {
    let h = Harness::new();
    let agent = FakeAgent::new(vec![
        FakeStep::AskPermission {
            title: "Edit draft".into(),
        },
        FakeStep::AskPermission {
            title: "Edit draft".into(),
        },
        FakeStep::AskPermission {
            title: "Edit draft".into(),
        },
    ]);
    let handle = h.start(&agent, h.config().await, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    handle.prompt("go");
    for choice in [
        PermissionChoice::AllowOnce,
        PermissionChoice::AllowSession,
        PermissionChoice::Deny,
    ] {
        let events = until(&handle, &mut transcript, |e| {
            permission_request(e).is_some()
        })
        .await;
        let (id, kind) = events.iter().find_map(permission_request).unwrap();
        assert_eq!(kind, PermissionKind::Tool);
        handle.respond(id, choice);
    }
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let log = agent.log();
    let permissions: Vec<&String> = log
        .iter()
        .filter(|l| l.starts_with("permission:"))
        .collect();
    assert_eq!(
        permissions,
        [
            "permission:Edit draft:allow",
            "permission:Edit draft:always",
            "permission:Edit draft:reject"
        ]
    );
    assert_eq!(transcript.pending_permissions().count(), 0);
}

#[tokio::test]
async fn mazemail_commands_run_and_others_need_approval() {
    let h = Harness::new();
    let agent = FakeAgent::new(vec![
        FakeStep::Terminal {
            command: "mazemail search 'from:anna'".into(),
            args: vec![],
        },
        FakeStep::Terminal {
            command: "echo".into(),
            args: vec!["approved".into()],
        },
        FakeStep::Terminal {
            command: "echo".into(),
            args: vec!["again".into()],
        },
        FakeStep::Terminal {
            command: "rm -rf scratch".into(),
            args: vec![],
        },
    ]);
    let handle = h.start(&agent, h.config().await, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    handle.prompt("search");

    // `echo approved`: allowed for the session, so `echo again` runs without asking.
    let events = until(&handle, &mut transcript, |e| {
        permission_request(e).is_some()
    })
    .await;
    let (id, kind) = events.iter().find_map(permission_request).unwrap();
    assert_eq!(kind, PermissionKind::Command);
    handle.respond(id, PermissionChoice::AllowSession);
    // `rm -rf scratch`: denied.
    let events = until(&handle, &mut transcript, |e| {
        permission_request(e).is_some()
    })
    .await;
    let (id, _) = events.iter().find_map(permission_request).unwrap();
    handle.respond(id, PermissionChoice::Deny);
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;

    let log: Vec<String> = agent
        .log()
        .into_iter()
        .filter(|l| l.starts_with("terminal:"))
        .collect();
    assert_eq!(
        log,
        [
            "terminal:mazemail search 'from:anna':0:cli:search from:anna",
            "terminal:echo:0:approved",
            "terminal:echo:0:again",
            "terminal:rm -rf scratch:error:the user denied running this command",
        ]
    );
    let terminals: Vec<(&str, bool, &TerminalStatus)> = transcript
        .entries
        .iter()
        .filter_map(|e| match e {
            Entry::Terminal(t) => Some((t.command.as_str(), t.automatic, &t.status)),
            _ => None,
        })
        .collect();
    assert_eq!(terminals[0].0, "mazemail search 'from:anna'");
    assert!(terminals[0].1, "mazemail commands run automatically");
    assert!(
        terminals
            .iter()
            .any(|(c, _, s)| *c == "rm -rf scratch" && **s == TerminalStatus::Denied)
    );
    // One entry per command: the approved run took over its waiting entry.
    assert_eq!(terminals.len(), 4, "{terminals:?}");
    assert!(terminals.iter().any(|(c, _, s)| *c == "echo approved"
        && **s
            == TerminalStatus::Exited {
                code: Some(0),
                signal: None
            }));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn approved_commands_stay_sandboxed() {
    let h = Harness::new();
    let home = h.dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("secret.txt"), "secret").unwrap();
    let scratch = home.join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let secret = home.join("secret.txt");
    let agent = FakeAgent::new(vec![FakeStep::Terminal {
        command: "/bin/cat".into(),
        args: vec![secret.display().to_string()],
    }]);
    let mut config = h.config().await;
    config.cwd = scratch.clone();
    config.terminal_policy = Some(sandbox::SandboxPolicy {
        hide: vec![home.clone()],
        write: vec![scratch],
        ..sandbox::SandboxPolicy::default()
    });
    let handle = h.start(&agent, config, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    handle.prompt("read it");
    let events = until(&handle, &mut transcript, |e| {
        permission_request(e).is_some()
    })
    .await;
    let (id, _) = events.iter().find_map(permission_request).unwrap();
    handle.respond(id, PermissionChoice::AllowOnce);
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let line = agent
        .log()
        .into_iter()
        .find(|l| l.starts_with("terminal:/bin/cat"))
        .unwrap();
    assert!(
        !line.contains(":0:"),
        "the sandbox must refuse the read: {line}"
    );
    assert!(!line.ends_with("secret"), "{line}");
}

#[tokio::test]
async fn sign_in_comes_before_the_session() {
    let h = Harness::new();
    let mut agent = FakeAgent::new(vec![FakeStep::Message("hi".into())]);
    agent.require_auth = true;
    let handle = h.start(&agent, h.config().await, SessionResources::default());
    let mut transcript = Transcript::default();
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::SignInRequired(_))
    })
    .await;
    let Some(SessionEvent::SignInRequired(request)) = events.last() else {
        panic!("expected a sign-in request")
    };
    assert_eq!(request.methods[0].id, "fake-login");
    assert_eq!(request.methods[0].name, "Sign in with Fake");
    assert_eq!(request.methods[0].terminal, None);
    handle.authenticate("fake-login");
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let log = agent.log();
    assert_eq!(
        &log[1..],
        [
            "new_session:auth_required".to_owned(),
            "authenticate:fake-login".to_owned(),
            format!("new_session:{}", h.dir.display()),
        ]
    );
}

#[tokio::test]
async fn resuming_loads_the_session_without_duplicating_it() {
    let h = Harness::new();
    let mut agent = FakeAgent::new(vec![FakeStep::Message("again".into())]);
    agent.load_session = true;
    let mut config = h.config().await;
    config.resume_acp_session = Some("old-session".into());
    config
        .transcript
        .apply(&EntryUpdate::UserMessage("earlier".into()));
    let handle = h.start(&agent, config, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    assert!(agent.log().contains(&"load_session:old-session".to_owned()));
    // The replayed history is not shown again.
    assert!(transcript.entries.is_empty(), "{:?}", transcript.entries);
}

#[tokio::test]
async fn teardown_revokes_the_token_and_deletes_the_scratch_dir() {
    let h = Harness::new();
    let paths = AgentPaths::new(&h.dir);
    let scratch = Scratch::create(&paths, "s_end", "# AGENTS").unwrap();
    let scratch_dir = scratch.dir().to_owned();
    let minted = crate::tokens::mint_session_token(
        h.host.tokens().as_ref(),
        h.host.account_policy().as_ref(),
        Profile::Triage,
        "test",
        "s_end",
    )
    .await
    .unwrap();
    let agent = FakeAgent::new(vec![]);
    let mut config = h.config().await;
    config.cwd = scratch_dir.clone();
    let handle = h.start(
        &agent,
        config,
        SessionResources {
            scratch: Some(scratch),
            token: Some((h.host.token_store(), minted.record.id.clone())),
            stderr: None,
            lease: None,
        },
    );
    let events = handle.events();
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    drop(handle);
    let ended = tokio::time::timeout(TIMEOUT, async {
        loop {
            if let SessionEvent::Ended(error) = events.recv().await.unwrap() {
                return error;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(ended, None);
    assert!(!scratch_dir.exists(), "the scratch dir is deleted");
    let record = h.host.tokens().list().await.unwrap().remove(0);
    assert!(record.revoked_at.is_some(), "the session token is revoked");
}

#[tokio::test]
async fn a_failed_prompt_asks_for_a_terminal_login_like_claude_code() {
    let h = Harness::new();
    let mut agent = FakeAgent::new(vec![FakeStep::Message("hi".into())]);
    agent.require_auth_at_prompt = true;
    agent.terminal_login = true;
    agent.auth_message = Some("Not logged in. Please run /login".into());
    let handle = h.start(&agent, h.config().await, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    handle.prompt("hello");
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::SignInRequired(_))
    })
    .await;
    assert!(events.contains(&SessionEvent::Status(SessionStatus::SignInRequired)));
    let Some(SessionEvent::SignInRequired(request)) = events.last() else {
        panic!("expected a sign-in request")
    };
    assert_eq!(
        request.message.as_deref(),
        Some("Not logged in. Please run /login")
    );
    let terminal = request
        .methods
        .iter()
        .find(|m| m.id == "fake-terminal")
        .expect("the terminal login is offered once Maze says it can run one");
    assert_eq!(
        terminal.terminal,
        Some(crate::TerminalAuth {
            command: None,
            args: vec!["--login".to_owned()],
            env: BTreeMap::new(),
        })
    );
    // The agent method still works in the running session.
    handle.authenticate("fake-login");
    until(&handle, &mut transcript, |e| {
        *e == SessionEvent::SignIn(crate::SignInProgress::Succeeded)
    })
    .await;
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    handle.prompt("hello again");
    until(&handle, &mut transcript, |e| {
        *e == SessionEvent::Status(SessionStatus::Working)
    })
    .await;
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    assert!(agent.log().contains(&"prompt:hello again".to_owned()));
}

#[tokio::test]
async fn links_the_agent_opens_while_signing_in_are_opened_for_it() {
    let h = Harness::new();
    let urls = h.dir.join(".open-urls");
    let mut agent = FakeAgent::new(vec![]);
    agent.require_auth = true;
    agent.browser_login = Some("https://cursor.example/login?challenge=abc".into());
    agent.open_urls = Some(urls.clone());
    let mut config = h.config().await;
    config.open_urls = Some(urls);
    let handle = h.start(&agent, config, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::SignInRequired(_))
    })
    .await;
    handle.authenticate("fake-login");
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Link { .. })
    })
    .await;
    assert!(
        events.contains(&SessionEvent::SignIn(crate::SignInProgress::Started(
            "fake-login".into()
        )))
    );
    assert_eq!(
        events.last(),
        Some(&SessionEvent::Link {
            url: "https://cursor.example/login?challenge=abc".into(),
            open: true,
        })
    );
    agent.finish_browser_login();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    assert!(agent.log().contains(&"browser_login:done".to_owned()));
    // The link is not kept in the conversation.
    assert!(transcript.entries.is_empty(), "{:?}", transcript.entries);
}

#[tokio::test]
async fn a_saved_key_signs_in_through_the_api_key_method() {
    let h = Harness::new();
    let mut agent = FakeAgent::new(vec![]);
    agent.require_auth = true;
    agent.api_key_method = true;
    let mut config = h.config().await;
    config.key_env = Some("OPENAI_API_KEY".into());
    let handle = h.start(&agent, config, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let log = agent.log();
    assert_eq!(
        &log[1..],
        [
            "new_session:auth_required".to_owned(),
            "authenticate:api-key".to_owned(),
            format!("new_session:{}", h.dir.display()),
        ]
    );
}

#[tokio::test]
async fn sign_in_errors_reach_the_ui() {
    let h = Harness::new();
    let urls = h.dir.join(".open-urls");
    let mut agent = FakeAgent::new(vec![]);
    agent.require_auth = true;
    agent.auth_message =
        Some("Authentication required. Please run 'cursor-agent login' first.".into());
    // A browser login whose "open" file can't be written fails.
    agent.browser_login = Some("https://cursor.example/login".into());
    agent.open_urls = Some(h.dir.join("missing-dir/urls"));
    let mut config = h.config().await;
    config.open_urls = Some(urls);
    let handle = h.start(&agent, config, SessionResources::default());
    let mut transcript = Transcript::default();
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::SignInRequired(_))
    })
    .await;
    let Some(SessionEvent::SignInRequired(request)) = events.last() else {
        panic!()
    };
    assert_eq!(
        request.message.as_deref(),
        Some("Authentication required. Please run 'cursor-agent login' first.")
    );
    handle.authenticate("fake-login");
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::SignIn(crate::SignInProgress::Failed(_)))
    })
    .await;
    assert!(matches!(
        events.last(),
        Some(SessionEvent::SignIn(crate::SignInProgress::Failed(message))) if !message.is_empty()
    ));
    // Still waiting for a sign-in; ending the session works.
    handle.shutdown();
    until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Ended(_))
    })
    .await;
}

fn last_controls(events: &[SessionEvent]) -> Vec<crate::Control> {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            SessionEvent::Controls(controls) => Some(controls.clone()),
            _ => None,
        })
        .expect("no controls")
}

fn current(controls: &[crate::Control], id: &str) -> Option<crate::ControlValue> {
    controls
        .iter()
        .find(|c| c.id == id)
        .map(crate::Control::value)
}

#[tokio::test]
async fn config_options_are_shown_changed_and_followed() {
    use crate::ControlValue::{Choice, Toggle};
    let h = Harness::new();
    let mut agent = FakeAgent::new(vec![FakeStep::SwitchModel("opus".into())]);
    agent.config_options = true;
    let handle = h.start(&agent, h.config().await, SessionResources::default());
    let mut transcript = Transcript::default();
    let events = until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let controls = last_controls(&events);
    // The config options win over the modes the agent also sent.
    let ids: Vec<&str> = controls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids, ["mode", "model", "effort", "fast"]);
    assert!(controls.iter().all(|c| !c.legacy_mode));
    assert_eq!(current(&controls, "model"), Some(Choice("sonnet".into())));

    // Picking Haiku: the agent drops the effort option.
    handle.set_control("model", Choice("haiku".into()));
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Controls(_))
    })
    .await;
    let controls = last_controls(&events);
    assert_eq!(current(&controls, "model"), Some(Choice("haiku".into())));
    assert_eq!(current(&controls, "effort"), None);
    handle.set_control("fast", Toggle(true));
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Controls(_))
    })
    .await;
    assert_eq!(current(&last_controls(&events), "fast"), Some(Toggle(true)));
    // A choice the agent doesn't offer is refused without asking it.
    handle.set_control("model", Choice("gpt-5".into()));
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Controls(_))
    })
    .await;
    assert_eq!(
        current(&last_controls(&events), "model"),
        Some(Choice("haiku".into()))
    );

    // The agent changes the model itself.
    handle.prompt("go");
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Controls(_))
    })
    .await;
    let controls = last_controls(&events);
    assert_eq!(current(&controls, "model"), Some(Choice("opus".into())));
    assert_eq!(current(&controls, "effort"), Some(Choice("high".into())));
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let log: Vec<String> = agent
        .log()
        .into_iter()
        .filter(|l| l.starts_with("set_"))
        .collect();
    assert_eq!(log, ["set_config:model=haiku", "set_config:fast=true"]);
    assert!(
        transcript.entries.iter().any(
            |e| matches!(e, crate::Entry::Notice { text, .. } if text.contains("doesn't offer"))
        )
    );
}

#[tokio::test]
async fn remembered_choices_are_applied_when_a_session_starts() {
    use crate::ControlValue::{Choice, Toggle};
    let h = Harness::new();
    let mut agent = FakeAgent::new(vec![]);
    agent.config_options = true;
    let mut config = h.config().await;
    config.preferred = BTreeMap::from([
        ("model".to_owned(), Choice("opus".into())),
        ("effort".to_owned(), Choice("low".into())),
        ("fast".to_owned(), Toggle(false)),
        ("gone".to_owned(), Choice("x".into())),
    ]);
    let handle = h.start(&agent, config, SessionResources::default());
    let mut transcript = Transcript::default();
    until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Controls(c) if current(c, "model") == Some(Choice("opus".into())))
    })
    .await;
    let controls = last_controls(&events);
    assert_eq!(current(&controls, "effort"), Some(Choice("low".into())));
    let log: Vec<String> = agent
        .log()
        .into_iter()
        .filter(|l| l.starts_with("set_"))
        .collect();
    // Only what differs, and only what the agent offers.
    assert_eq!(log, ["set_config:effort=low", "set_config:model=opus"]);
}

#[tokio::test]
async fn agents_with_only_modes_get_a_mode_control() {
    use crate::ControlValue::Choice;
    let h = Harness::new();
    let mut agent = FakeAgent::new(vec![FakeStep::SwitchMode("autoEdit".into())]);
    agent.modes = true;
    let handle = h.start(&agent, h.config().await, SessionResources::default());
    let mut transcript = Transcript::default();
    let events = until(&handle, &mut transcript, is_status(SessionStatus::Ready)).await;
    let controls = last_controls(&events);
    assert_eq!(controls.len(), 1);
    assert!(controls[0].legacy_mode);
    handle.set_control("mode", Choice("yolo".into()));
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Controls(_))
    })
    .await;
    assert_eq!(
        current(&last_controls(&events), "mode"),
        Some(Choice("yolo".into()))
    );
    handle.prompt("go");
    let events = until(&handle, &mut transcript, |e| {
        matches!(e, SessionEvent::Controls(_))
    })
    .await;
    assert_eq!(
        current(&last_controls(&events), "mode"),
        Some(Choice("autoEdit".into()))
    );
    assert!(agent.log().contains(&"set_mode:yolo".to_owned()));
}

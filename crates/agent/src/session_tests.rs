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
            terminal_login: Some("HOME=/x fake-agent".to_owned()),
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
    assert_eq!(log[0], "initialize:terminal=true:fs_read=false");
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
    let Some(SessionEvent::SignInRequired(methods)) = events.last() else {
        panic!("expected a sign-in request")
    };
    assert_eq!(methods[0].id, "fake-login");
    assert_eq!(methods[0].name, "Sign in with Fake");
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

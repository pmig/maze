//! The panel against the scripted in-process agent and an in-memory host. Sessions
//! run on a real Tokio runtime, so tests wait by polling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent::fake::{FakeAgent, FakeStep};
use agent::http::tests::MemoryHttp;
use agent::{
    AgentHost as _, AgentManager, AgentPaths, Entry, ManagerConfig, MemoryHost, PermissionChoice,
    ProposalItemStatus, ProposalItemView, ProposalView, SessionConfig, SessionResources,
    SessionStatus, StartedSession, Transcript, start_with_transport,
};
use gpui::{Entity, TestAppContext, VisualTestContext};
use mazemail_types::{AccountId, ThreadId};
use settings::Settings;

use crate::{AgentPanel, Launcher, MailContext};

struct Harness {
    runtime: tokio::runtime::Runtime,
    host: Arc<MemoryHost>,
    http: Arc<MemoryHttp>,
    manager: Arc<AgentManager>,
    launcher: Arc<Recorder>,
    open_urls: Option<PathBuf>,
    preferred: BTreeMap<String, agent::ControlValue>,
    dir: tempfile::TempDir,
}

/// Records links and terminal logins instead of opening a browser or Terminal.
#[derive(Debug, Default)]
struct Recorder {
    urls: Mutex<Vec<String>>,
    scripts: Mutex<Vec<PathBuf>>,
}

impl Launcher for Recorder {
    fn open_url(&self, url: &str, _: &mut gpui::App) {
        self.urls.lock().unwrap().push(url.to_owned());
    }

    fn open_terminal(&self, script: &Path) -> std::io::Result<()> {
        self.scripts.lock().unwrap().push(script.to_owned());
        Ok(())
    }
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let host = Arc::new(MemoryHost::new([AccountId(1)]));
        let http = Arc::new(MemoryHttp::default());
        let manager = Arc::new(AgentManager::new(ManagerConfig {
            paths: AgentPaths::new(dir.path()),
            home: None,
            cli_program: None,
            socket: None,
            host: Arc::clone(&host) as Arc<dyn agent::AgentHost>,
            http: Arc::clone(&http) as Arc<dyn agent::Http>,
        }));
        Self {
            runtime,
            host,
            http,
            manager,
            launcher: Arc::default(),
            open_urls: None,
            preferred: BTreeMap::new(),
            dir,
        }
    }

    fn open<'a>(
        &self,
        cx: &'a mut TestAppContext,
    ) -> (Entity<AgentPanel>, &'a mut VisualTestContext) {
        self.open_with(Settings::default(), cx)
    }

    fn open_with<'a>(
        &self,
        settings: Settings,
        cx: &'a mut TestAppContext,
    ) -> (Entity<AgentPanel>, &'a mut VisualTestContext) {
        cx.executor().allow_parking();
        let handle = self.runtime.handle().clone();
        cx.update(|cx| {
            ui::init(cx);
            ui::gpui_tokio::init_from_handle(cx, handle);
            // The window's own actions aren't linked into this test; ours must all bind.
            let errors = ui::keymap::bind_keymap(ui::keymap::DEFAULT_MACOS_KEYMAP, cx).unwrap();
            let ours: Vec<_> = errors
                .iter()
                .filter(|e| e.to_string().contains("agent::"))
                .collect();
            assert!(ours.is_empty(), "agent key bindings failed: {ours:?}");
        });
        let manager = Arc::clone(&self.manager);
        let launcher = Arc::clone(&self.launcher) as Arc<dyn Launcher>;
        cx.add_window_view(|window, cx| {
            let mut panel = AgentPanel::new(manager, settings, window, cx);
            panel.set_launcher(launcher);
            panel
        })
    }

    /// A session with the fake agent, as the manager would start it.
    fn session(&self, agent: &FakeAgent) -> StartedSession {
        let _runtime = self.runtime.enter();
        let key = self
            .runtime
            .block_on(
                self.host
                    .create_agent_thread("fake", None, Some(ThreadId(3))),
            )
            .unwrap();
        let handle = start_with_transport(
            agent.transport(),
            SessionConfig {
                session_id: "s_ui".to_owned(),
                agent_id: "fake".to_owned(),
                agent_name: "Fake".to_owned(),
                agent_thread: key,
                resume_acp_session: None,
                cwd: std::env::temp_dir(),
                terminal_env: BTreeMap::new(),
                cli_program: None,
                terminal_policy: None,
                attached: None,
                transcript: Transcript::default(),
                open_urls: self.open_urls.clone(),
                key_env: None,
                preferred: self.preferred.clone(),
            },
            Arc::clone(&self.host) as Arc<dyn agent::AgentHost>,
            SessionResources::default(),
            None,
        );
        StartedSession {
            handle,
            transcript: Transcript::default(),
            attached: None,
            context_note: None,
            sandboxed: true,
            policy_description: Some("Hidden: ~".to_owned()),
            key_env: None,
        }
    }
}

/// Runs the app until `done` holds.
fn wait_until(
    cx: &mut VisualTestContext,
    what: &str,
    mut done: impl FnMut(&mut VisualTestContext) -> bool,
) {
    for _ in 0..2000 {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

fn status(panel: &Entity<AgentPanel>, cx: &mut VisualTestContext) -> Option<SessionStatus> {
    panel.read_with(cx, |panel, _| panel.session_status())
}

fn entries(panel: &Entity<AgentPanel>, cx: &mut VisualTestContext) -> Vec<Entry> {
    panel.read_with(cx, |panel, _| {
        panel
            .transcript()
            .map(|t| t.entries.clone())
            .unwrap_or_default()
    })
}

fn type_prompt(panel: &Entity<AgentPanel>, text: &str, cx: &mut VisualTestContext) {
    let focus = panel.read_with(cx, |panel, cx| {
        use gpui::Focusable as _;
        panel.prompt().focus_handle(cx)
    });
    cx.update(|window, cx| window.focus(&focus, cx));
    cx.simulate_input(text);
}

#[gpui::test]
fn the_conversation_streams_in_and_enter_sends(cx: &mut TestAppContext) {
    let h = Harness::new();
    let (panel, cx) = h.open(cx);
    let agent = FakeAgent::new(vec![
        FakeStep::Thought("Reading".into()),
        FakeStep::ToolCall {
            id: "t1".into(),
            title: "Read thread.md".into(),
        },
        FakeStep::ToolDone { id: "t1".into() },
        FakeStep::Message("Anna proposes ".into()),
        FakeStep::Message("**Friday**.".into()),
    ]);
    let started = h.session(&agent);
    panel.update(cx, |panel, cx| {
        panel.attach_session(started, "Fake", false, cx)
    });
    wait_until(cx, "the session", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready)
    });

    // shift-enter is a new line, enter sends.
    type_prompt(&panel, "Summarize", cx);
    cx.simulate_keystrokes("shift-enter");
    cx.simulate_input("briefly");
    cx.simulate_keystrokes("enter");
    wait_until(cx, "the answer", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready)
            && entries(&panel, cx)
                .iter()
                .any(|e| matches!(e, Entry::Agent { text } if text == "Anna proposes **Friday**."))
    });
    let entries = entries(&panel, cx);
    assert_eq!(
        entries[0],
        Entry::User {
            text: "Summarize\nbriefly".into()
        }
    );
    assert!(matches!(&entries[1], Entry::Thought { text } if text == "Reading"));
    assert!(
        matches!(&entries[2], Entry::ToolCall(call) if call.status == agent::ToolStatus::Completed)
    );
    // The prompt was cleared.
    let prompt = panel.read_with(cx, |panel, cx| panel.prompt().read(cx).value().to_string());
    assert_eq!(prompt, "");
    assert!(agent.log().iter().any(|l| l == "prompt:Summarize\nbriefly"));
}

#[gpui::test]
fn permission_prompts_offer_three_choices(cx: &mut TestAppContext) {
    let h = Harness::new();
    let (panel, cx) = h.open(cx);
    let agent = FakeAgent::new(vec![
        FakeStep::AskPermission {
            title: "Save draft".into(),
        },
        FakeStep::AskPermission {
            title: "Save draft".into(),
        },
        FakeStep::AskPermission {
            title: "Save draft".into(),
        },
    ]);
    let started = h.session(&agent);
    panel.update(cx, |panel, cx| {
        panel.attach_session(started, "Fake", false, cx)
    });
    wait_until(cx, "the session", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready)
    });
    type_prompt(&panel, "draft a reply", cx);
    cx.simulate_keystrokes("enter");
    for choice in [
        PermissionChoice::AllowOnce,
        PermissionChoice::AllowSession,
        PermissionChoice::Deny,
    ] {
        let mut prompt = None;
        wait_until(cx, "a permission prompt", |cx| {
            prompt = panel.read_with(cx, |panel, _| {
                panel
                    .transcript()
                    .and_then(|t| t.pending_permissions().next().cloned())
            });
            prompt.is_some()
        });
        let prompt = prompt.unwrap();
        assert_eq!(prompt.title, "Save draft");
        assert_eq!(prompt.choices.len(), 3);
        // Click the button, as a user would.
        let selector = match choice {
            PermissionChoice::AllowOnce => "agent-permission-allow-once",
            PermissionChoice::AllowSession => "agent-permission-allow-session",
            PermissionChoice::Deny => "agent-permission-deny",
        };
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} isn't rendered"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        wait_until(cx, "the decision", |cx| {
            panel.read_with(cx, |panel, _| {
                panel
                    .transcript()
                    .is_some_and(|t| t.pending_permissions().all(|p| p.id != prompt.id))
            })
        });
    }
    wait_until(cx, "the turn", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready)
            && agent
                .log()
                .iter()
                .filter(|l| l.starts_with("permission:"))
                .count()
                == 3
    });
    let decided: Vec<String> = agent
        .log()
        .into_iter()
        .filter(|l| l.starts_with("permission:"))
        .collect();
    assert_eq!(
        decided,
        [
            "permission:Save draft:allow",
            "permission:Save draft:always",
            "permission:Save draft:reject"
        ]
    );
}

fn proposal(id: i64, title: &str, items: &[(i64, &str)]) -> ProposalView {
    ProposalView {
        id,
        title: title.to_owned(),
        source: Some("Maze agent session: Fake".to_owned()),
        items: items
            .iter()
            .map(|(id, summary)| ProposalItemView {
                id: *id,
                summary: (*summary).to_owned(),
                reason: None,
                status: ProposalItemStatus::Pending,
            })
            .collect(),
    }
}

fn item_status(h: &Harness, item: i64) -> ProposalItemStatus {
    h.host
        .proposals()
        .iter()
        .flat_map(|p| p.items.clone())
        .find(|i| i.id == item)
        .unwrap()
        .status
}

#[gpui::test]
fn proposals_are_reviewed_with_y_n_and_accept_all(cx: &mut TestAppContext) {
    let h = Harness::new();
    h.host.add_proposal(proposal(
        1,
        "Archive newsletters",
        &[(11, "Archive 3 messages"), (12, "Archive 1 message")],
    ));
    h.host
        .add_proposal(proposal(2, "Tag To Do", &[(21, "Tag 2 messages")]));
    let (panel, cx) = h.open(cx);
    panel.update(cx, |panel, cx| {
        panel.set_context(
            Some(MailContext {
                thread: ThreadId(3),
                subject: "Q3 offsite".into(),
            }),
            cx,
        );
    });
    wait_until(cx, "the proposals", |cx| {
        panel.read_with(cx, |panel, _| panel.attention_count()) == 3
    });

    // Focus the review (as a click on an item does), then y and n.
    let focus = panel.read_with(cx, |panel, _| panel.review_focus_for_tests());
    cx.update(|window, cx| window.focus(&focus, cx));
    // Newest proposal first: "Tag To Do" (21), then 11, 12.
    cx.simulate_keystrokes("y");
    wait_until(cx, "the first decision", |_| {
        item_status(&h, 21) == ProposalItemStatus::Applied
    });
    wait_until(cx, "the refresh", |cx| {
        panel.read_with(cx, |panel, _| panel.attention_count()) == 2
    });
    cx.simulate_keystrokes("n");
    wait_until(cx, "the rejection", |_| {
        item_status(&h, 11) == ProposalItemStatus::Rejected
    });
    wait_until(cx, "the refresh", |cx| {
        panel.read_with(cx, |panel, _| panel.attention_count()) == 1
    });

    // cmd-shift-enter accepts everything left.
    cx.simulate_keystrokes("cmd-shift-enter");
    wait_until(cx, "accept all", |_| {
        item_status(&h, 12) == ProposalItemStatus::Applied
    });
    wait_until(cx, "an empty review", |cx| {
        panel.read_with(cx, |panel, _| panel.proposals().is_empty())
    });
}

#[gpui::test]
fn the_picker_lists_built_in_agents_and_the_pi_notice(cx: &mut TestAppContext) {
    let h = Harness::new();
    let (panel, cx) = h.open(cx);
    cx.run_until_parked();
    let ids: Vec<String> = panel.read_with(cx, |panel, _| {
        panel.catalog().iter().map(|s| s.entry.id.clone()).collect()
    });
    assert_eq!(
        &ids[..5],
        ["claude-acp", "codex-acp", "gemini", "cursor", "pi-acp"]
    );
    assert_eq!(
        panel.read_with(cx, |panel, _| panel.selected_agent().to_owned()),
        "claude-acp"
    );
    let pi = panel.read_with(cx, |panel, _| {
        panel
            .catalog()
            .iter()
            .find(|s| s.entry.id == "pi-acp")
            .and_then(|s| s.entry.notice)
    });
    assert_eq!(pi, Some(agent::PI_NOTICE));
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    cx.run_until_parked();
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} isn't rendered"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
}

fn rendered(cx: &mut VisualTestContext, selector: &'static str) -> bool {
    cx.run_until_parked();
    cx.debug_bounds(selector).is_some()
}

/// Settings with the custom agent "fake" (started as the in-process fake agent).
fn fake_agent_settings() -> Settings {
    let mut settings = Settings::default();
    settings.agent.custom.insert(
        "fake".to_owned(),
        settings::CustomAgentSettings {
            name: Some("Fake".to_owned()),
            command: "/bin/sh".to_owned(),
            args: vec!["--acp".to_owned()],
            env: BTreeMap::new(),
        },
    );
    settings.agent_sandbox.enabled = false;
    settings
}

#[gpui::test]
fn sign_in_offers_every_method_a_terminal_login_and_a_key(cx: &mut TestAppContext) {
    const KEY: &str = "sk-proj-PanelKeyCanary";
    let h = Harness::new();
    // Every start of "fake" runs a fresh fake agent; remember each launch's environment.
    let launches: Arc<Mutex<Vec<BTreeMap<String, String>>>> = Arc::default();
    let agents: Arc<Mutex<Vec<FakeAgent>>> = Arc::default();
    {
        let launches = Arc::clone(&launches);
        let agents = Arc::clone(&agents);
        h.manager.set_test_agents(Arc::new(move |spec| {
            launches.lock().unwrap().push(spec.env.clone());
            let mut agent = FakeAgent::new(vec![FakeStep::Message("hi".into())]);
            agent.require_auth = true;
            agent.terminal_login = true;
            agent.api_key_method = true;
            agent.auth_message = Some("Please sign in to Fake first.".into());
            agents.lock().unwrap().push(agent.clone());
            Some(agent)
        }));
    }
    let (panel, cx) = h.open_with(fake_agent_settings(), cx);
    panel.update(cx, |panel, cx| panel.start_for_tests("fake", cx));
    wait_until(cx, "the sign-in card", |cx| {
        status(&panel, cx) == Some(SessionStatus::SignInRequired) && rendered(cx, "agent-sign-in")
    });
    // One button per method: the terminal login, and the agent's own two.
    for selector in [
        "agent-sign-in-status",
        "agent-sign-in-message",
        "agent-sign-in-terminal-0",
        "agent-sign-in-method-0",
        "agent-sign-in-method-1",
        "agent-key-input",
        "agent-key-save",
    ] {
        assert!(rendered(cx, selector), "{selector} isn't rendered");
    }

    // "Sign in…" writes the login script and opens it in Terminal.
    click(cx, "agent-sign-in-terminal-0");
    wait_until(cx, "Terminal", |cx| {
        rendered(cx, "agent-sign-in-retry") && !h.launcher.scripts.lock().unwrap().is_empty()
    });
    let script_path = h.launcher.scripts.lock().unwrap()[0].clone();
    let script = std::fs::read_to_string(&script_path).unwrap();
    let agent_home = h.dir.path().join("agent-home/fake");
    assert!(script.contains("'/bin/sh' '--acp' '--login'"), "{script}");
    assert!(script.contains(&format!("'HOME={}'", agent_home.display())));
    assert!(
        !script.contains("mazemail_s_"),
        "no session token in the script"
    );
    assert!(h.launcher.urls.lock().unwrap().is_empty());

    // Paste a key: it is saved encrypted, and the agent starts again with it.
    let input = panel.read_with(cx, |panel, _| panel.key_input_for_tests());
    let focus = input.read_with(cx, |input, cx| {
        use gpui::Focusable as _;
        input.focus_handle(cx)
    });
    cx.update(|window, cx| window.focus(&focus, cx));
    cx.simulate_input(&format!(" {KEY} "));
    click(cx, "agent-key-save");
    wait_until(cx, "the second start", |cx| {
        launches.lock().unwrap().len() == 2 && status(&panel, cx) == Some(SessionStatus::Ready)
    });
    let second = launches.lock().unwrap()[1].clone();
    assert_eq!(second.get("OPENAI_API_KEY").map(String::as_str), Some(KEY));
    assert!(!launches.lock().unwrap()[0].contains_key("OPENAI_API_KEY"));
    let saved = h.host.key("fake").unwrap();
    assert_eq!(saved.env, "OPENAI_API_KEY");
    assert_eq!(saved.key.expose(), KEY);
    // The agent took it through its API-key method without another click.
    let log = agents.lock().unwrap()[1].log();
    assert!(log.contains(&"authenticate:api-key".to_owned()), "{log:?}");
    assert!(rendered(cx, "agent-signed-in"));
    // The key never reaches the conversation, and the input is cleared.
    let json = panel.read_with(cx, |panel, _| panel.transcript().unwrap().to_json());
    assert!(!String::from_utf8_lossy(&json).contains("PanelKeyCanary"));
    assert_eq!(
        input.read_with(cx, |input, _| input.value().to_string()),
        ""
    );
}

#[gpui::test]
fn links_the_agent_opens_during_sign_in_open_in_the_browser(cx: &mut TestAppContext) {
    let mut h = Harness::new();
    let urls = h.dir.path().join(".open-urls");
    h.open_urls = Some(urls.clone());
    let (panel, cx) = h.open(cx);
    let mut agent = FakeAgent::new(vec![]);
    agent.require_auth = true;
    agent.browser_login = Some("https://cursor.example/login?c=1".into());
    agent.open_urls = Some(urls);
    let started = h.session(&agent);
    panel.update(cx, |panel, cx| {
        panel.attach_session(started, "Cursor", false, cx)
    });
    wait_until(cx, "the sign-in card", |cx| {
        rendered(cx, "agent-sign-in-method-0")
    });
    click(cx, "agent-sign-in-method-0");
    wait_until(cx, "the browser", |cx| {
        !h.launcher.urls.lock().unwrap().is_empty() && rendered(cx, "agent-sign-in-link-0")
    });
    assert_eq!(
        *h.launcher.urls.lock().unwrap(),
        ["https://cursor.example/login?c=1"]
    );
    assert!(rendered(cx, "agent-sign-in-pending"));
    agent.finish_browser_login();
    wait_until(cx, "the session", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready)
    });
    assert!(rendered(cx, "agent-signed-in"));
}

#[gpui::test]
fn a_prompt_without_a_login_shows_the_sign_in_card(cx: &mut TestAppContext) {
    let h = Harness::new();
    let (panel, cx) = h.open(cx);
    let mut agent = FakeAgent::new(vec![FakeStep::Message("hi".into())]);
    agent.require_auth_at_prompt = true;
    agent.terminal_login = true;
    agent.auth_message = Some("Not logged in. Please run /login".into());
    let started = h.session(&agent);
    panel.update(cx, |panel, cx| {
        panel.attach_session(started, "Claude Code", false, cx)
    });
    wait_until(cx, "the session", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready)
    });
    type_prompt(&panel, "Summarize", cx);
    cx.simulate_keystrokes("enter");
    wait_until(cx, "the sign-in card", |cx| {
        status(&panel, cx) == Some(SessionStatus::SignInRequired)
            && rendered(cx, "agent-sign-in-message")
            && rendered(cx, "agent-sign-in-terminal-0")
    });
}

fn control_value(
    panel: &Entity<AgentPanel>,
    id: &str,
    cx: &mut VisualTestContext,
) -> Option<agent::ControlValue> {
    panel.read_with(cx, |panel, _| {
        panel
            .controls()
            .iter()
            .find(|c| c.id == id)
            .map(agent::Control::value)
    })
}

#[gpui::test]
fn the_prompt_bar_picks_model_mode_and_thinking(cx: &mut TestAppContext) {
    use agent::ControlValue::{Choice, Toggle};
    let h = Harness::new();
    let (panel, cx) = h.open(cx);
    let mut agent = FakeAgent::new(vec![FakeStep::SwitchModel("haiku".into())]);
    agent.config_options = true;
    let started = h.session(&agent);
    panel.update(cx, |panel, cx| {
        panel.attach_session(started, "Claude Code", false, cx)
    });
    wait_until(cx, "the controls", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready) && rendered(cx, "agent-control-model")
    });
    for selector in [
        "agent-prompt-bar",
        "agent-control-mode",
        "agent-control-model",
        "agent-control-thinking",
        "agent-control-fast",
    ] {
        assert!(rendered(cx, selector), "{selector} isn't rendered");
    }

    // Zed's keys in the prompt.
    let set = |agent: &FakeAgent| -> Vec<String> {
        agent
            .log()
            .into_iter()
            .filter(|l| l.starts_with("set_"))
            .collect()
    };
    type_prompt(&panel, "", cx);
    cx.simulate_keystrokes("shift-tab");
    wait_until(cx, "the mode", |_| set(&agent).len() == 1);
    cx.simulate_keystrokes("ctrl-'");
    wait_until(cx, "the effort", |_| set(&agent).len() == 2);
    cx.simulate_keystrokes("alt-tab");
    wait_until(cx, "the model", |_| set(&agent).len() == 3);
    assert_eq!(
        set(&agent),
        [
            "set_config:mode=acceptEdits",
            "set_config:effort=low",
            "set_config:model=opus"
        ]
    );
    // A click on the switch.
    click(cx, "agent-control-fast");
    wait_until(cx, "fast", |cx| {
        control_value(&panel, "fast", cx) == Some(Toggle(true)) && set(&agent).len() == 4
    });
    // Remembered for the agent's next sessions.
    let remembered = h.manager.preferences().get("fake");
    assert_eq!(remembered.get("model"), Some(&Choice("opus".into())));
    assert_eq!(remembered.get("mode"), Some(&Choice("acceptEdits".into())));
    assert_eq!(remembered.get("fast"), Some(&Toggle(true)));

    // The agent switches the model itself: the bar follows (Haiku has no effort).
    type_prompt(&panel, "go", cx);
    cx.simulate_keystrokes("enter");
    wait_until(cx, "the agent's switch", |cx| {
        control_value(&panel, "model", cx) == Some(Choice("haiku".into()))
            && status(&panel, cx) == Some(SessionStatus::Ready)
    });
    assert!(!rendered(cx, "agent-control-thinking"));
}

#[gpui::test]
fn remembered_choices_apply_to_new_conversations(cx: &mut TestAppContext) {
    use agent::ControlValue::Choice;
    let h = Harness::new();
    let agents: Arc<Mutex<Vec<FakeAgent>>> = Arc::default();
    {
        let agents = Arc::clone(&agents);
        h.manager.set_test_agents(Arc::new(move |_| {
            let mut agent = FakeAgent::new(vec![]);
            agent.config_options = true;
            agents.lock().unwrap().push(agent.clone());
            Some(agent)
        }));
    }
    let (panel, cx) = h.open_with(fake_agent_settings(), cx);
    panel.update(cx, |panel, cx| panel.start_for_tests("fake", cx));
    wait_until(cx, "the first session", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready) && rendered(cx, "agent-control-model")
    });
    panel.update(cx, |panel, cx| {
        panel.set_control("model", &Choice("opus".into()), cx)
    });
    wait_until(cx, "the switch", |_| {
        agents.lock().unwrap()[0]
            .log()
            .contains(&"set_config:model=opus".to_owned())
    });
    cx.simulate_keystrokes("cmd-n");
    panel.update(cx, |panel, cx| panel.start_for_tests("fake", cx));
    wait_until(cx, "the second session", |cx| {
        agents.lock().unwrap().len() == 2
            && control_value(&panel, "model", cx) == Some(Choice("opus".into()))
    });
    assert!(
        agents.lock().unwrap()[1]
            .log()
            .contains(&"set_config:model=opus".to_owned())
    );
}

#[gpui::test]
fn agents_with_only_modes_show_a_mode_picker(cx: &mut TestAppContext) {
    use agent::ControlValue::Choice;
    let h = Harness::new();
    let (panel, cx) = h.open(cx);
    let mut agent = FakeAgent::new(vec![]);
    agent.modes = true;
    let started = h.session(&agent);
    panel.update(cx, |panel, cx| {
        panel.attach_session(started, "Gemini CLI", false, cx)
    });
    wait_until(cx, "the mode picker", |cx| {
        rendered(cx, "agent-control-mode")
    });
    assert!(!rendered(cx, "agent-control-model"));
    type_prompt(&panel, "", cx);
    cx.simulate_keystrokes("shift-tab");
    wait_until(cx, "the mode", |cx| {
        agent.log().contains(&"set_mode:autoEdit".to_owned())
            && control_value(&panel, "mode", cx) == Some(Choice("autoEdit".into()))
    });
}

fn binary_registry(version: &str, url: &str) -> agent::registry::Registry {
    use agent::registry::{BinaryTarget, Distribution, Registry, RegistryAgent};
    let mut targets = BTreeMap::new();
    if let Some(key) = agent::registry::platform_key() {
        targets.insert(
            key.to_owned(),
            BinaryTarget {
                archive: url.to_owned(),
                sha256: None,
                cmd: "./cursor-agent".to_owned(),
                args: vec!["acp".to_owned()],
                env: BTreeMap::new(),
            },
        );
    }
    Registry {
        version: "1.0.0".to_owned(),
        agents: vec![RegistryAgent {
            id: "cursor".to_owned(),
            name: "Cursor".to_owned(),
            version: version.to_owned(),
            description: None,
            repository: None,
            website: None,
            authors: Vec::new(),
            license: None,
            icon: None,
            distribution: Distribution {
                binary: Some(targets),
                npx: None,
                uvx: None,
            },
        }],
    }
}

#[gpui::test]
fn installed_agents_update_in_the_background(cx: &mut TestAppContext) {
    if agent::registry::platform_key().is_none() {
        return;
    }
    let h = Harness::new();
    h.http
        .insert("https://downloads.example/cursor-agent-1", b"#!/bin/sh\n");
    h.http
        .insert("https://downloads.example/cursor-agent-2", b"#!/bin/sh\n");
    h.manager.set_registry(binary_registry(
        "1.0.0",
        "https://downloads.example/cursor-agent-1",
    ));
    h.runtime
        .block_on(h.manager.install("cursor", &Settings::default(), &|_| {}))
        .unwrap();
    // The registry now lists 2.0.0.
    let newer = binary_registry("2.0.0", "https://downloads.example/cursor-agent-2");
    h.http.insert(
        agent::registry::REGISTRY_URL,
        &serde_json::to_vec(&newer).unwrap(),
    );
    let (panel, cx) = h.open(cx);
    panel.update(cx, |panel, cx| panel.start_auto_updates(cx));
    wait_until(cx, "the update", |cx| {
        panel.read_with(cx, |panel, _| {
            panel.updated_for_tests("cursor").as_deref() == Some("2.0.0")
                && panel.catalog().iter().any(|s| {
                    s.entry.id == "cursor"
                        && s.installed.as_ref().map(|i| i.version.as_str()) == Some("2.0.0")
                        && !s.update_available()
                })
        })
    });
    let paths = AgentPaths::new(h.dir.path());
    assert!(paths.install_dir("cursor", "2.0.0").is_dir());
    assert!(
        !paths.install_dir("cursor", "1.0.0").exists(),
        "no session used it"
    );
}

#[gpui::test]
fn the_thread_looks_like_zeds(cx: &mut TestAppContext) {
    let h = Harness::new();
    let (panel, cx) = h.open(cx);
    let agent = FakeAgent::new(vec![
        FakeStep::Thought("Reading the thread".into()),
        FakeStep::Plan(vec!["Read".into(), "Answer".into()]),
        FakeStep::ToolCall {
            id: "t1".into(),
            title: "Read thread.md".into(),
        },
        FakeStep::ToolDone { id: "t1".into() },
        FakeStep::Message("Done.".into()),
    ]);
    let started = h.session(&agent);
    panel.update(cx, |panel, cx| {
        panel.attach_session(started, "Fake", false, cx)
    });
    wait_until(cx, "the empty thread", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready) && rendered(cx, "agent-empty")
    });
    for selector in ["agent-title", "agent-new-thread", "agent-prompt-bar"] {
        assert!(rendered(cx, selector), "{selector} isn't rendered");
    }
    type_prompt(&panel, "Summarize", cx);
    cx.simulate_keystrokes("enter");
    wait_until(cx, "the answer", |cx| {
        status(&panel, cx) == Some(SessionStatus::Ready) && entries(&panel, cx).len() == 5
    });
    // User bubble, collapsed thinking, plan card, compact tool row, plain answer.
    for selector in [
        "agent-user-message-0",
        "agent-thinking-1",
        "agent-plan",
        "agent-tool-3",
        "agent-answer-4",
    ] {
        assert!(rendered(cx, selector), "{selector} isn't rendered");
    }
    assert!(!rendered(cx, "agent-empty"));
    assert!(!rendered(cx, "agent-working"));
}

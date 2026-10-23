//! The panel against the scripted in-process agent and an in-memory host. Sessions
//! run on a real Tokio runtime, so tests wait by polling.

use std::collections::BTreeMap;
use std::sync::Arc;
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

use crate::{AgentPanel, MailContext};

struct Harness {
    runtime: tokio::runtime::Runtime,
    host: Arc<MemoryHost>,
    manager: Arc<AgentManager>,
    _dir: tempfile::TempDir,
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
        let manager = Arc::new(AgentManager::new(ManagerConfig {
            paths: AgentPaths::new(dir.path()),
            home: None,
            cli_program: None,
            socket: None,
            host: Arc::clone(&host) as Arc<dyn agent::AgentHost>,
            http: Arc::new(MemoryHttp::default()),
        }));
        Self {
            runtime,
            host,
            manager,
            _dir: dir,
        }
    }

    fn open<'a>(
        &self,
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
        cx.add_window_view(|window, cx| AgentPanel::new(manager, Settings::default(), window, cx))
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
                terminal_login: None,
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

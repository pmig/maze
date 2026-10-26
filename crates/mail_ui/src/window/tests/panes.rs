//! Tabs and splits in the main window (ADR-018).

use gpui::{FocusHandle, Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, TestAppContext};
use ui::panes::{Item, ItemEvent};

use super::*;

/// What the app opens for a draft (the composer); here a stand-in with the same tab
/// behavior: titled with its two-word name, asking before a tab with changes closes.
struct DraftItem {
    name: SharedString,
    dirty: bool,
    focus: FocusHandle,
}

impl DraftItem {
    fn new(name: String, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self {
            name: name.into(),
            dirty: false,
            focus: cx.focus_handle(),
        })
    }
}

impl EventEmitter<ItemEvent> for DraftItem {}

impl Focusable for DraftItem {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DraftItem {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .key_context("Composer")
            .track_focus(&self.focus)
            .child(self.name.clone())
    }
}

impl Item for DraftItem {
    fn tab_title(&self, _: &App) -> SharedString {
        self.name.clone()
    }

    fn is_dirty(&self, _: &App) -> bool {
        self.dirty
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        if !self.dirty {
            return true;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Save this message as a draft?",
            None,
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            if let Ok(0 | 1) = answer.await {
                this.update(cx, |_, cx| cx.emit(ItemEvent::Close)).ok();
            }
        })
        .detach();
        false
    }

    fn serialize(&self, _: &App) -> Option<SerializedItem> {
        Some(
            SerializedItem::new("compose", serde_json::json!({ "draft": 1 }))
                .with_title(self.name.to_string()),
        )
    }
}

/// Does what the app does for a [`ComposeRequest::New`]: a draft tab named by the
/// window.
fn open_draft(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> Entity<DraftItem> {
    let draft = view.update_in(cx, |w, window, cx| {
        let name = w.new_draft_name(cx);
        let draft = DraftItem::new(name, cx);
        w.add_item(Box::new(draft.clone()), window, cx);
        draft
    });
    cx.run_until_parked();
    draft
}

fn titles(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> Vec<Vec<String>> {
    view.read_with(cx, |w, cx| {
        w.panes()
            .read(cx)
            .panes()
            .iter()
            .map(|pane| {
                pane.read(cx)
                    .items()
                    .iter()
                    .map(|item| item.tab_title(cx).to_string())
                    .collect()
            })
            .collect()
    })
}

fn focus_list(view: &Entity<MailWindow>, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        let list = view.read(cx).thread_list(cx).unwrap();
        list.update(cx, |list, cx| list.focus(window, cx));
    });
    // Focus listeners (the active pane follows the focus) run when the window draws.
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

#[gpui::test]
fn the_window_starts_with_an_inbox_tab(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    assert_eq!(titles(&view, cx), [["Inbox · Unified"]]);
    assert!(
        cx.debug_bounds("pane-tab-0").is_some(),
        "the tab bar is drawn"
    );
}

#[gpui::test]
fn cmd_n_asks_for_a_draft_that_opens_in_the_active_pane(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let requests: Rc<RefCell<Vec<ComposeRequest>>> = Rc::default();
    let seen = requests.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&view, move |_, request: &ComposeRequest, _| {
            seen.borrow_mut().push(request.clone());
        })
    });
    // A split first: the draft goes to the active (new, right) pane.
    focus_list(&view, cx);
    cx.simulate_keystrokes("cmd-\\");
    cx.run_until_parked();
    assert_eq!(titles(&view, cx).len(), 2);
    cx.simulate_keystrokes("cmd-n");
    assert_eq!(*requests.borrow(), [ComposeRequest::New]);
    let first = open_draft(&view, cx);
    let second = open_draft(&view, cx);
    let tabs = titles(&view, cx);
    assert_eq!(tabs[0].len(), 1, "the left pane keeps its tab");
    assert_eq!(tabs[1].len(), 3);
    let names = [first, second].map(|d| d.read_with(cx, |d, _| d.name.to_string()));
    for name in &names {
        assert!(ui::names::is_two_word_name(name), "{name}");
    }
    assert_ne!(names[0], names[1], "open drafts have different names");
    assert_eq!(tabs[1][2], names[1]);
    let active = view.read_with(cx, |w, cx| {
        w.panes()
            .read(cx)
            .active_item(cx)
            .map(|item| item.tab_title(cx).to_string())
    });
    assert_eq!(active.as_deref(), Some(names[1].as_str()));
}

#[gpui::test]
fn double_click_and_enter_open_the_message_in_a_tab(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let bounds = cx.debug_bounds("card-row-2").expect("a row");
    let click = |cx: &mut VisualTestContext, click_count| {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: bounds.center(),
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: bounds.center(),
            modifiers: Modifiers::none(),
            click_count,
        });
    };
    click(cx, 1);
    click(cx, 2);
    cx.run_until_parked();
    let tab = view
        .read_with(cx, |w, cx| w.active_message_tab(cx))
        .expect("a message tab is active");
    let shown = view.read_with(cx, |w, cx| {
        w.panes().read(cx).items_of::<MailView>(cx)[0]
            .read(cx)
            .shown_message(cx)
    });
    assert_eq!(Some(tab.read_with(cx, |t, _| t.message())), shown);
    let tabs = titles(&view, cx);
    assert_eq!(tabs[0].len(), 2);
    assert_ne!(tabs[0][1], "Message", "titled with the subject");
    // The agent column attaches the message of the active tab.
    let (context, thread) = view.read_with(cx, |w, cx| {
        (
            w.agent_context(cx).map(|(thread, _)| thread),
            tab.read(cx).view().read(cx).document().map(|d| d.thread),
        )
    });
    assert_eq!(context, thread);

    // Back to the list: `enter` opens the next message, a second tab.
    cx.simulate_keystrokes("cmd-1");
    focus_list(&view, cx);
    cx.simulate_keystrokes("f enter");
    cx.run_until_parked();
    assert_eq!(titles(&view, cx)[0].len(), 3);
    // The same message again only activates its tab.
    cx.simulate_keystrokes("cmd-1");
    focus_list(&view, cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(titles(&view, cx)[0].len(), 3);
    // `cmd-w` closes the message tab.
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert_eq!(titles(&view, cx)[0].len(), 2);
}

#[gpui::test]
fn actions_go_to_the_focused_mail_view(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    focus_list(&view, cx);
    cx.simulate_keystrokes("cmd-\\");
    cx.run_until_parked();
    let views = view.read_with(cx, |w, cx| w.panes().read(cx).items_of::<MailView>(cx));
    assert_eq!(views.len(), 2, "the split shows a second mail view");
    let len = |cx: &mut VisualTestContext, ix: usize| {
        views[ix].read_with(cx, |v, cx| v.thread_list().read(cx).len())
    };
    let before = len(cx, 0);
    assert_eq!(len(cx, 1), before);
    // The new (right) view has the focus: `a` archives there only.
    cx.simulate_keystrokes("a");
    cx.run_until_parked();
    assert_eq!(len(cx, 1), before - 1);
    assert_eq!(len(cx, 0), before);
    cx.simulate_keystrokes("cmd-z");
    cx.run_until_parked();
    assert_eq!(len(cx, 1), before);

    // Over to the left view; picking a folder opens it there.
    cx.update(|window, cx| {
        let list = views[0].read(cx).thread_list().clone();
        list.update(cx, |list, cx| list.focus(window, cx));
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    view.update_in(cx, |w, window, cx| {
        let pane = w.folder_pane.clone();
        let event = FolderPaneEvent::Selected {
            key: FakeMailSource::large_folder(),
            title: "Archives".into(),
        };
        w.on_folder_pane_event(&pane, &event, window, cx);
    });
    assert_eq!(len(cx, 0), 1_000_000);
    assert_eq!(len(cx, 1), before);
    assert_eq!(titles(&view, cx)[0], ["Archives"]);
}

#[gpui::test]
fn folders_open_in_new_tabs_and_tabs_select_their_folder(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    view.update_in(cx, |w, window, cx| {
        let pane = w.folder_pane.clone();
        let event = FolderPaneEvent::OpenInNewTab {
            key: FakeMailSource::large_folder(),
            title: "Archives".into(),
        };
        w.on_folder_pane_event(&pane, &event, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(titles(&view, cx), [["Inbox · Unified", "Archives"]]);
    let selected = |cx: &mut VisualTestContext| {
        view.read_with(cx, |w, cx| w.folder_pane().read(cx).selected().cloned())
    };
    assert_eq!(selected(cx), Some(FakeMailSource::large_folder()));
    cx.simulate_keystrokes("cmd-1");
    cx.run_until_parked();
    assert_eq!(selected(cx), Some(FolderKey::Unified(FolderRole::Inbox)));
}

#[gpui::test]
fn closing_a_draft_with_changes_asks_first(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let draft = open_draft(&view, cx);
    draft.update(cx, |d, cx| {
        d.dirty = true;
        cx.emit(ItemEvent::UpdateTab);
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("tab-dirty-1").is_some(),
        "a dot marks the changes"
    );
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(titles(&view, cx)[0].len(), 2, "still open");
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    assert_eq!(titles(&view, cx), [["Inbox · Unified"]]);
}

/// Stands in for the agent panel.
struct AgentPanel;

impl Render for AgentPanel {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .debug_selector(|| "agent-panel".into())
            .size_full()
            .child("Agent")
    }
}

#[gpui::test]
fn the_agent_column_stays_right_of_the_splits(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.update(|window, _| window.resize(size(px(1680.), px(1000.))));
    let panel = cx.update(|_, cx| cx.new(|_| AgentPanel));
    view.update(cx, |w, cx| w.set_agent_panel(panel.into(), cx));
    cx.run_until_parked();
    focus_list(&view, cx);
    let draft = open_draft(&view, cx);
    // Drag the draft's tab onto the right edge of its pane.
    view.update_in(cx, |w, window, cx| {
        let panes = w.panes().clone();
        panes.update(cx, |panes, cx| {
            let pane = panes.active_pane().clone();
            panes.move_item(
                &pane,
                draft.entity_id(),
                &pane,
                ui::panes::DropTarget::Split(ui::panes::SplitDirection::Right),
                window,
                cx,
            );
        });
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert_eq!(titles(&view, cx).len(), 2);
    let group = cx.debug_bounds("pane-group").expect("the panes");
    let agent = cx.debug_bounds("agent-panel").expect("the agent column");
    assert!(
        group.right() <= agent.left(),
        "panes {group:?}, agent {agent:?}"
    );
}

#[gpui::test]
fn the_title_bar_button_hides_and_shows_the_folder_pane(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let folders = "folder-pane-row:unified:inbox";
    assert!(cx.debug_bounds(folders).is_some());
    let button = cx.debug_bounds("toggle-folder-pane").expect("the button");
    cx.simulate_click(button.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(!view.read_with(cx, |w, _| w.prefs().folder_pane_visible));
    assert!(cx.debug_bounds(folders).is_none(), "hidden");
    cx.simulate_click(button.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(view.read_with(cx, |w, _| w.prefs().folder_pane_visible));
    assert!(cx.debug_bounds(folders).is_some());
}

#[gpui::test]
fn the_layout_comes_back_when_the_window_reopens(cx: &mut TestAppContext) {
    cx.update(|cx| {
        ui::init(cx);
        crate::init(cx);
    });
    let ui_state = UiStateStore::in_memory(ListDefaults::default());
    let source: Arc<dyn MailSource> = Arc::new(FakeMailSource::with_now(
        time::macros::datetime!(2026-10-22 10:12 UTC),
    ));
    let options = MailWindowOptions {
        ui_state: ui_state.clone(),
        ..MailWindowOptions::default()
    };
    let (view, vcx) = {
        let source = source.clone();
        let options = options.clone();
        cx.add_window_view(move |window, cx| MailWindow::new(source, &options, window, cx))
    };
    vcx.update(|window, _| window.activate_window());
    vcx.run_until_parked();
    focus_list(&view, vcx);
    vcx.simulate_keystrokes("enter");
    vcx.run_until_parked();
    let message = view
        .read_with(vcx, |w, cx| w.active_message_tab(cx))
        .map(|tab| tab.read_with(vcx, |t, _| t.message()))
        .expect("a message tab");
    let draft = open_draft(&view, vcx);
    let name = draft.read_with(vcx, |d, _| d.name.to_string());
    vcx.simulate_keystrokes("cmd-1");
    focus_list(&view, vcx);
    vcx.simulate_keystrokes("cmd-\\");
    vcx.run_until_parked();
    view.update(vcx, |w, cx| w.save_layout(cx));
    let before = titles(&view, vcx);
    assert_eq!(before.len(), 2);
    assert_eq!(before[0].len(), 3);
    assert!(before[0].contains(&name));

    // Draft 1 has no restorer here (the app registers it): it waits, keeping its name.
    let (reopened, rcx) =
        cx.add_window_view(move |window, cx| MailWindow::new(source, &options, window, cx));
    rcx.run_until_parked();
    let after = titles(&reopened, rcx);
    assert_eq!(after[1], before[1]);
    assert_eq!(after[0][0], before[0][0]);
    assert_eq!(after[0][1], "Message", "subjects aren't kept on disk");
    assert_eq!(after[0][2], name, "the draft keeps its name");
    let restored = reopened.read_with(rcx, |w, cx| {
        let panes = w.panes().read(cx);
        (
            panes.items_of::<MessageTab>(cx).len(),
            panes.panes().iter().position(|p| p == panes.active_pane()),
        )
    });
    assert_eq!(restored.1, Some(1), "the right pane is active again");
    // The message tab isn't shown: it is restored when it is.
    assert_eq!(restored.0, 0);
    rcx.update(|window, cx| {
        let pane = reopened.read(cx).panes().read(cx).panes()[0].clone();
        pane.update(cx, |pane, cx| pane.activate(1, true, window, cx));
    });
    rcx.run_until_parked();
    let tab = reopened
        .read_with(rcx, |w, cx| w.panes().read(cx).items_of::<MessageTab>(cx))
        .pop()
        .expect("restored");
    assert_eq!(tab.read_with(rcx, |t, _| t.message()), message);
    assert!(ui_state.layout().is_some());
}

#[gpui::test]
fn every_default_binding_names_a_known_action(cx: &mut TestAppContext) {
    cx.update(|cx| {
        ui::init(cx);
        let file = ui::keymap::KeymapFile::parse(ui::keymap::DEFAULT_MACOS_KEYMAP).unwrap();
        let loaded = file.load(cx);
        let unknown: Vec<String> = loaded
            .errors
            .iter()
            .map(|error| error.to_string())
            // The composer's and the agent column's actions live in other crates.
            .filter(|error| !error.contains("composer::") && !error.contains("agent::"))
            .collect();
        assert!(unknown.is_empty(), "{unknown:#?}");
    });
}

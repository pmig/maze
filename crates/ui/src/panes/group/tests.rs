use gpui::{FocusHandle, Focusable, SharedString, TestAppContext, VisualTestContext};
use serde_json::json;

use super::*;
use crate::panes::{ItemEvent, SerializedAxis};

/// A tab for tests: a title, maybe unsaved changes.
struct TestItem {
    title: SharedString,
    dirty: bool,
    focus: FocusHandle,
}

impl TestItem {
    fn new(title: &str, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self {
            title: title.to_owned().into(),
            dirty: false,
            focus: cx.focus_handle(),
        })
    }
}

impl EventEmitter<ItemEvent> for TestItem {}

impl Focusable for TestItem {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TestItem {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.focus)
            .child(self.title.clone())
    }
}

impl Item for TestItem {
    fn tab_title(&self, _: &App) -> SharedString {
        self.title.clone()
    }

    fn is_dirty(&self, _: &App) -> bool {
        self.dirty
    }

    fn request_close(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> bool {
        !self.dirty
    }

    fn serialize(&self, _: &App) -> Option<SerializedItem> {
        Some(SerializedItem::new("test", json!({ "title": self.title })))
    }

    fn clone_on_split(&self, _: &mut Window, cx: &mut Context<'_, Self>) -> Option<Entity<Self>> {
        let title = self.title.clone();
        Some(cx.new(|cx| Self {
            title,
            dirty: false,
            focus: cx.focus_handle(),
        }))
    }
}

/// A window around a pane group, binding the pane actions like Maze's window does.
struct Harness {
    group: Entity<PaneGroup>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        crate::panes::bind_actions(div().size_full().child(self.group.clone()), &self.group)
    }
}

fn open(cx: &mut TestAppContext) -> (Entity<PaneGroup>, &mut VisualTestContext) {
    cx.update(|cx| {
        crate::init(cx);
        let keymap = r#"[{ "bindings": {
            "cmd-w": "pane::CloseActiveItem",
            "cmd-\\": "pane::SplitRight",
            "ctrl-tab": "pane::ActivateNextItem",
            "cmd-1": ["pane::ActivateItem", 0]
        } }]"#;
        let errors = crate::keymap::bind_keymap(keymap, cx).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    });
    let (harness, cx) = cx.add_window_view(|window, cx| Harness {
        group: cx.new(|cx| PaneGroup::new(None, window, cx)),
    });
    let group = harness.read_with(cx, |h, _| h.group.clone());
    cx.run_until_parked();
    (group, cx)
}

fn add(group: &Entity<PaneGroup>, title: &str, cx: &mut VisualTestContext) -> Entity<TestItem> {
    let item = cx.update(|_, cx| TestItem::new(title, cx));
    let boxed: Box<dyn ItemHandle> = Box::new(item.clone());
    group.update_in(cx, |g, window, cx| g.add_item(boxed, true, window, cx));
    cx.run_until_parked();
    item
}

fn titles(group: &Entity<PaneGroup>, cx: &mut VisualTestContext) -> Vec<Vec<String>> {
    group.read_with(cx, |g, cx| {
        g.panes()
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

fn active_title(group: &Entity<PaneGroup>, cx: &mut VisualTestContext) -> Option<String> {
    group.read_with(cx, |g, cx| {
        g.active_item(cx).map(|item| item.tab_title(cx).to_string())
    })
}

#[gpui::test]
fn tabs_open_after_the_active_one_and_keys_switch_them(cx: &mut TestAppContext) {
    let (group, cx) = open(cx);
    add(&group, "Inbox", cx);
    add(&group, "brave-otter", cx);
    group.update_in(cx, |g, window, cx| g.activate_item_at(0, window, cx));
    add(&group, "Invoice", cx);
    assert_eq!(titles(&group, cx), [["Inbox", "Invoice", "brave-otter"]]);
    assert_eq!(active_title(&group, cx).as_deref(), Some("Invoice"));
    cx.simulate_keystrokes("ctrl-tab");
    assert_eq!(active_title(&group, cx).as_deref(), Some("brave-otter"));
    cx.simulate_keystrokes("cmd-1");
    assert_eq!(active_title(&group, cx).as_deref(), Some("Inbox"));
    // Closing goes back to the tab used before.
    cx.simulate_keystrokes("ctrl-tab cmd-w");
    assert_eq!(titles(&group, cx), [["Inbox", "brave-otter"]]);
    assert_eq!(active_title(&group, cx).as_deref(), Some("Inbox"));
}

#[gpui::test]
fn cmd_backslash_splits_with_a_copy(cx: &mut TestAppContext) {
    let (group, cx) = open(cx);
    add(&group, "Inbox", cx);
    let first = group.read_with(cx, |g, _| g.active_pane().clone());
    cx.simulate_keystrokes("cmd-\\");
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["Inbox"], ["Inbox"]]);
    let (second, ids_differ) = group.read_with(cx, |g, cx| {
        let panes = g.panes();
        let ids: Vec<_> = panes
            .iter()
            .map(|p| p.read(cx).items()[0].item_id())
            .collect();
        (g.active_pane().clone(), ids[0] != ids[1])
    });
    assert!(ids_differ, "the new pane shows a copy");
    assert_ne!(first, second, "the new pane is active");
    assert!(matches!(
        group.read_with(cx, |g, _| g.tree().root().clone()),
        Member::Split(Split {
            axis: Axis::Horizontal,
            ..
        })
    ));
}

#[gpui::test]
fn moving_tabs_between_panes_and_into_splits(cx: &mut TestAppContext) {
    let (group, cx) = open(cx);
    add(&group, "Inbox", cx);
    let compose = add(&group, "brave-otter", cx);
    let left = group.read_with(cx, |g, _| g.active_pane().clone());

    // Dropped on the right edge: a new pane on the right.
    group.update_in(cx, |g, window, cx| {
        g.move_item(
            &left,
            compose.entity_id(),
            &left,
            DropTarget::Split(SplitDirection::Right),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["Inbox"], ["brave-otter"]]);
    let right = group.read_with(cx, |g, _| g.active_pane().clone());
    assert_ne!(left, right);

    // Onto the left pane's first tab: the right pane, now empty, closes.
    group.update_in(cx, |g, window, cx| {
        g.move_item(
            &right,
            compose.entity_id(),
            &left,
            DropTarget::Tab(0),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["brave-otter", "Inbox"]]);
    assert_eq!(group.read_with(cx, |g, _| g.active_pane().clone()), left);

    // Reordering within a pane.
    group.update_in(cx, |g, window, cx| {
        g.move_item(
            &left,
            compose.entity_id(),
            &left,
            DropTarget::Tab(2),
            window,
            cx,
        )
    });
    assert_eq!(titles(&group, cx), [["Inbox", "brave-otter"]]);

    // Below, then into the middle of the other pane.
    group.update_in(cx, |g, window, cx| {
        g.move_item(
            &left,
            compose.entity_id(),
            &left,
            DropTarget::Split(SplitDirection::Down),
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["Inbox"], ["brave-otter"]]);
    assert!(matches!(
        group.read_with(cx, |g, _| g.tree().root().clone()),
        Member::Split(Split {
            axis: Axis::Vertical,
            ..
        })
    ));
    let lower = group.read_with(cx, |g, _| g.active_pane().clone());
    group.update_in(cx, |g, window, cx| {
        g.move_item(
            &lower,
            compose.entity_id(),
            &left,
            DropTarget::Center,
            window,
            cx,
        )
    });
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["Inbox", "brave-otter"]]);

    // A pane's only tab onto its own edge changes nothing.
    let inbox = group.read_with(cx, |g, cx| g.items(cx)[0].item_id());
    group.update_in(cx, |g, window, cx| {
        g.move_item(&left, inbox, &left, DropTarget::Center, window, cx);
    });
    assert_eq!(titles(&group, cx), [["Inbox", "brave-otter"]]);
}

#[gpui::test]
fn a_dirty_item_decides_when_its_tab_closes(cx: &mut TestAppContext) {
    let (group, cx) = open(cx);
    add(&group, "Inbox", cx);
    let draft = add(&group, "brave-otter", cx);
    draft.update(cx, |d, cx| {
        d.dirty = true;
        cx.emit(ItemEvent::UpdateTab);
    });
    cx.run_until_parked();
    assert!(cx.debug_bounds("tab-dirty-1").is_some(), "a dot on the tab");
    cx.simulate_keystrokes("cmd-w");
    assert_eq!(titles(&group, cx), [["Inbox", "brave-otter"]], "kept");
    // The item closes itself once the user answered.
    draft.update(cx, |_, cx| cx.emit(ItemEvent::Close));
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["Inbox"]]);
}

#[gpui::test]
fn the_last_pane_stays_when_its_last_tab_closes(cx: &mut TestAppContext) {
    let (group, cx) = open(cx);
    add(&group, "Inbox", cx);
    cx.simulate_keystrokes("cmd-\\");
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["Inbox"]], "the empty pane closed");
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [Vec::<String>::new()]);
}

fn test_kind(cx: &mut VisualTestContext, group: &Entity<PaneGroup>) {
    group.update_in(cx, |g, window, cx| {
        g.register_kind(
            "test",
            ItemKind::new(
                |item, _, cx| match item.data.get("title").and_then(|t| t.as_str()) {
                    Some("gone") => Restore::Gone,
                    Some("later") => Restore::Later,
                    Some(title) => Restore::Item(Box::new(TestItem::new(title, cx))),
                    None => Restore::Gone,
                },
            )
            .title(|item, _| {
                item.data
                    .get("title")
                    .and_then(|t| t.as_str())
                    .map(|t| SharedString::from(t.to_owned()))
            }),
            window,
            cx,
        );
    });
}

fn item(title: &str) -> SerializedItem {
    SerializedItem::new("test", json!({ "title": title }))
}

#[gpui::test]
fn layouts_round_trip_and_restore_lazily(cx: &mut TestAppContext) {
    let (group, cx) = open(cx);
    test_kind(cx, &group);
    let layout = SerializedLayout {
        root: SerializedMember::Split {
            axis: SerializedAxis::Horizontal,
            members: vec![
                SerializedMember::Pane(SerializedPane {
                    items: vec![item("Inbox"), item("gone"), item("Archive")],
                    active: 0,
                }),
                SerializedMember::Pane(SerializedPane {
                    items: vec![item("later"), item("Sent")],
                    active: 0,
                }),
                // Nothing to show: dropped.
                SerializedMember::Pane(SerializedPane::default()),
            ],
            flexes: vec![2., 1., 1.],
        },
        active_pane: 1,
    };
    assert!(group.update_in(cx, |g, window, cx| g.load(layout, window, cx)));
    cx.run_until_parked();
    assert_eq!(
        titles(&group, cx),
        [vec!["Inbox", "gone", "Archive"], vec!["later", "Sent"]],
        "titles before restoring come from the kind"
    );
    let pending = |cx: &mut VisualTestContext| {
        group.read_with(cx, |g, cx| g.items_of::<PendingItem>(cx).len())
    };
    assert_eq!(
        pending(cx),
        4,
        "only the active tabs are restored (and \"later\" waits)"
    );
    assert_eq!(active_title(&group, cx).as_deref(), Some("later"));

    // A tab whose content is gone disappears when it is shown.
    let left = group.read_with(cx, |g, _| g.panes()[0].clone());
    left.update_in(cx, |p, window, cx| p.activate(1, false, window, cx));
    cx.run_until_parked();
    assert_eq!(titles(&group, cx)[0], ["Inbox", "Archive"]);
    let (active, archive_pending) = group.read_with(cx, |g, cx| {
        let pane = g.panes()[0].read(cx);
        (pane.active_index(), pane.items()[1].is::<PendingItem>())
    });
    assert_eq!(active, 0, "back to the tab shown before");
    assert!(archive_pending, "Archive wasn't shown yet");

    let saved = group.read_with(cx, |g, cx| g.serialize(cx));
    let SerializedMember::Split {
        flexes, members, ..
    } = &saved.root
    else {
        panic!("a split");
    };
    assert_eq!(flexes, &[2., 1.]);
    assert_eq!(members.len(), 2);
    assert_eq!(saved.active_pane, 1);
    let json = serde_json::to_string(&saved).unwrap();
    let back: SerializedLayout = serde_json::from_str(&json).unwrap();
    assert_eq!(back, saved);

    // A layout without tabs changes nothing.
    let empty = SerializedLayout {
        root: SerializedMember::Pane(SerializedPane::default()),
        active_pane: 0,
    };
    assert!(!group.update_in(cx, |g, window, cx| g.load(empty, window, cx)));
    assert_eq!(group.read_with(cx, |g, _| g.panes().len()), 2);
}

#[gpui::test]
fn tabs_wait_for_their_kind(cx: &mut TestAppContext) {
    let (group, cx) = open(cx);
    let layout = SerializedLayout {
        root: SerializedMember::Pane(SerializedPane {
            items: vec![item("Drafts").with_title("brave-otter")],
            active: 0,
        }),
        active_pane: 0,
    };
    group.update_in(cx, |g, window, cx| g.load(layout, window, cx));
    cx.run_until_parked();
    assert_eq!(titles(&group, cx), [["brave-otter"]], "the stored title");
    test_kind(cx, &group);
    cx.run_until_parked();
    assert!(group.read_with(cx, |g, cx| g.items_of::<PendingItem>(cx).is_empty()));
    assert_eq!(titles(&group, cx), [["Drafts"]]);
}

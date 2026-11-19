//! Dragging mail from the list onto a folder in the folder pane: moved there as Quick
//! Move moves it (the selection when the grabbed row is in it, else that row), and
//! nothing on a unified folder or on the folder the mail is in.

use gpui::{Modifiers, MouseButton, Point, TestAppContext};

use super::*;
use crate::fake::MoveItem;

fn open_list(
    cx: &mut TestAppContext,
) -> (
    Entity<MailWindow>,
    Arc<FakeMailSource>,
    &mut VisualTestContext,
) {
    cx.update(|cx| {
        ui::init(cx);
        crate::init(cx);
    });
    let fake = Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
    let source: Arc<dyn MailSource> = fake.clone();
    let (view, cx) = cx.add_window_view(|window, cx| {
        MailWindow::new(source, &MailWindowOptions::default(), window, cx)
    });
    cx.run_until_parked();
    (view, fake, cx)
}

fn leak(text: String) -> &'static str {
    Box::leak(text.into_boxed_str())
}

/// Drags `from` onto `to` with the mouse.
fn drag(cx: &mut VisualTestContext, from: &'static str, to: &'static str) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let start = cx
        .debug_bounds(from)
        .unwrap_or_else(|| panic!("{from} is on screen"))
        .center();
    let target = cx
        .debug_bounds(to)
        .unwrap_or_else(|| panic!("{to} is on screen"))
        .center();
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + Point::new(gpui::px(10.), gpui::px(4.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_mouse_move(target, MouseButton::Left, Modifiers::none());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
}

/// The thread of row `ix` of the open list.
fn thread_at(view: &Entity<MailWindow>, ix: usize, cx: &mut VisualTestContext) -> ThreadId {
    let list = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap());
    let items = list.update(cx, |list, _| list.drag_items(ix));
    match items.as_slice() {
        [crate::thread_list::ItemId::Thread(id)] => *id,
        other => panic!("row {ix} is one thread, not {other:?}"),
    }
}

/// A favorite folder on screen in the folder pane, as (folder, row selector): the list
/// then shows the Inbox of its account, whose mail can move there.
fn inbox_next_to_a_favorite(
    view: &Entity<MailWindow>,
    fake: &FakeMailSource,
    cx: &mut VisualTestContext,
) -> (FolderId, &'static str) {
    fn ids(entries: &[crate::source::FolderEntry], out: &mut Vec<FolderId>) {
        for entry in entries {
            if let FolderKey::Folder(id) = entry.key {
                out.push(id);
            }
            ids(&entry.children, out);
        }
    }
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for account in &fake.folder_tree().accounts {
        let inbox = account.folders.iter().find_map(|f| match f.key {
            FolderKey::Folder(id) if f.role == Some(FolderRole::Inbox) => Some(id),
            _ => None,
        });
        let Some(inbox) = inbox else { continue };
        let mut folders = Vec::new();
        ids(&account.folders, &mut folders);
        let favorite = folders
            .into_iter()
            .filter(|id| *id != inbox)
            .find_map(|id| {
                let row = leak(format!("folder-pane-row:favorite:folder:{id}"));
                cx.debug_bounds(row).map(|_| (id, row))
            });
        if let Some(favorite) = favorite {
            view.update_in(cx, |w, window, cx| {
                let scope = ThreadScope::Folder(FolderKey::Folder(inbox));
                w.open_folder(scope, "Inbox".into(), None, false, window, cx);
            });
            cx.run_until_parked();
            return favorite;
        }
    }
    panic!("a favorite of an account with an Inbox is in the folder pane");
}

#[gpui::test]
fn dragging_the_selected_thread_onto_a_folder_moves_it(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_list(cx);
    let (folder, row) = inbox_next_to_a_favorite(&view, &fake, cx);
    let thread = thread_at(&view, 0, cx);

    drag(cx, "card-row-0", row);
    assert_eq!(fake.moved_to(MoveItem::Thread(thread)), Some(folder));
    let next = view.read_with(cx, |w, cx| {
        w.thread_list(cx).unwrap().read(cx).selected_thread()
    });
    assert_ne!(next, Some(thread), "the next thread took its place");
}

#[gpui::test]
fn a_selected_row_drags_the_selection_and_another_row_itself(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_list(cx);
    let (folder, row) = inbox_next_to_a_favorite(&view, &fake, cx);
    let list = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap());
    list.update(cx, |list, cx| list.select(0, cx));
    cx.run_until_parked();
    let (first, second, third) = (
        thread_at(&view, 0, cx),
        thread_at(&view, 1, cx),
        thread_at(&view, 2, cx),
    );

    // Row 2 isn't selected: only its thread moves.
    drag(cx, "card-row-2", row);
    assert_eq!(fake.moved_to(MoveItem::Thread(third)), Some(folder));
    assert_eq!(fake.moved_to(MoveItem::Thread(first)), None);

    // Rows 0 and 1 selected, row 1 grabbed: both move.
    list.update(cx, |list, cx| {
        list.select(0, cx);
        list.toggle_row(1, cx);
    });
    cx.run_until_parked();
    drag(cx, "card-row-1", row);
    assert_eq!(fake.moved_to(MoveItem::Thread(first)), Some(folder));
    assert_eq!(fake.moved_to(MoveItem::Thread(second)), Some(folder));
}

#[gpui::test]
fn a_unified_folder_or_the_mails_own_folder_takes_no_drop(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_list(cx);
    let thread = thread_at(&view, 0, cx);
    let (_, inbox) = super::trash_and_inbox(&fake, thread);

    drag(cx, "card-row-0", "folder-pane-row:unified:inbox");
    assert_eq!(fake.moved_to(MoveItem::Thread(thread)), None);

    // The account's own Inbox, which the unified Inbox shows the thread from.
    let own = leak(format!("folder-pane-row:folder:{inbox}"));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    if cx.debug_bounds(own).is_some() {
        drag(cx, "card-row-0", own);
        assert_eq!(fake.moved_to(MoveItem::Thread(thread)), None);
    }
}

#[gpui::test]
fn table_rows_drag_too(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_list(cx);
    let (folder, row) = inbox_next_to_a_favorite(&view, &fake, cx);
    let list = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap());
    cx.simulate_keystrokes("cmd-alt-t");
    assert_eq!(list.read_with(cx, |l, _| l.mode()), ListMode::Table);
    let thread = thread_at(&view, 1, cx);

    drag(cx, "table-row-1", row);
    assert_eq!(fake.moved_to(MoveItem::Thread(thread)), Some(folder));
}

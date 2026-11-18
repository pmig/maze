//! Saved searches in the window (ADR-030): saving the shown search, the folder pane's
//! section, opening one, editing and deleting it.

use gpui::TestAppContext;
use mazemail_types::SavedSearchId;

use super::*;

/// A window with the app's Root (dialogs need its layer) over a fake whose searches
/// leave out the 1M folder.
fn open_with_root(
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
    let fake = Arc::new(
        FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC))
            .without_large_folder_in_searches(),
    );
    let source: Arc<dyn MailSource> = fake.clone();
    let mut view = None;
    let window = cx.add_window(|window, cx| {
        let mail = cx.new(|cx| MailWindow::new(source, &MailWindowOptions::default(), window, cx));
        view = Some(mail.clone());
        gpui_kit::base::Root::new(mail, window, cx)
    });
    let view = view.unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    (view, fake, cx)
}

/// A word of the subject of the first row of the open list: it finds some mail.
fn some_word(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |w, _| {
        let inbox = ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox));
        let keys = w
            .source
            .thread_keys(
                &inbox,
                &crate::source::ThreadFilter::default(),
                ListSort::default(),
            )
            .unwrap();
        let first = keys[0];
        let row = w.source.thread_rows(&[first]).remove(0);
        row.subject
            .expose()
            .split_whitespace()
            .max_by_key(|word| word.len())
            .unwrap()
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_lowercase()
    })
}

fn scope(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> ThreadScope {
    view.read_with(cx, |w, cx| {
        w.thread_list(cx).unwrap().read(cx).scope().clone()
    })
}

fn list_len(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> usize {
    view.read_with(cx, |w, cx| w.thread_list(cx).unwrap().read(cx).key_count())
}

fn title(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |w, cx| {
        w.thread_list(cx).unwrap().read(cx).title().to_string()
    })
}

/// Types `name` (and, if given, `query`) into the open dialog and submits it.
fn fill_and_submit(
    view: &Entity<MailWindow>,
    name: &str,
    query: Option<&str>,
    cx: &mut VisualTestContext,
) -> bool {
    let saved = view.update_in(cx, |w, window, cx| {
        let dialog = w.saved_search_dialog().expect("the dialog is open").clone();
        let (name_input, query_input) = {
            let dialog = dialog.read(cx);
            (dialog.name.clone(), dialog.query.clone())
        };
        name_input.update(cx, |input, cx| input.set_value(name.to_owned(), window, cx));
        if let Some(query) = query {
            query_input.update(cx, |input, cx| {
                input.set_value(query.to_owned(), window, cx)
            });
        }
        w.submit_saved_search_dialog(window, cx)
    });
    cx.run_until_parked();
    saved
}

fn saved_names(fake: &FakeMailSource) -> Vec<String> {
    fake.folder_tree()
        .saved_searches
        .iter()
        .map(|s| s.name.to_string())
        .collect()
}

#[gpui::test]
fn saving_a_search_shows_it_in_the_pane_and_lists_its_results(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_with_root(cx);
    let word = some_word(&view, cx);
    view.update_in(cx, |w, window, cx| w.run_search(word.clone(), window, cx));
    cx.run_until_parked();
    let found = list_len(&view, cx);
    assert!(found > 0, "{word} finds the row it came from");

    // `cmd-alt-s` asks for a name, with the search's query filled in.
    cx.simulate_keystrokes("cmd-alt-s");
    cx.run_until_parked();
    let query = view.read_with(cx, |w, cx| {
        let dialog = w
            .saved_search_dialog()
            .expect("Save Search… opens the dialog");
        dialog.read(cx).query.read(cx).value().to_string()
    });
    assert_eq!(query, word);
    assert!(fill_and_submit(&view, "Mine", None, cx));
    assert_eq!(saved_names(&fake), ["Mine"]);
    let id = fake.folder_tree().saved_searches[0].id;
    let key = FolderKey::SavedSearch(id);

    // The pane shows it under Saved Searches, selected; the list shows its results.
    view.read_with(cx, |w, cx| {
        let pane = w.folder_pane().read(cx);
        assert_eq!(pane.selected(), Some(&key));
        assert_eq!(pane.title_of(&key).as_deref(), Some("Mine"));
        assert!(w.saved_search_dialog().is_none(), "the dialog closed");
    });
    assert_eq!(scope(&view, cx), ThreadScope::Folder(key.clone()));
    assert_eq!(title(&view, cx), "Mine");
    assert_eq!(list_len(&view, cx), found);
    // The layout keeps it by id.
    let kept = SerializedFolder::of(&ThreadScope::Folder(key.clone()));
    assert_eq!(kept, SerializedFolder::SavedSearch { id });
    assert_eq!(kept.key(), key);

    // Another search can't take its name (ignoring case): the dialog says why and
    // stays open.
    view.update_in(cx, |w, window, cx| w.run_search(word.clone(), window, cx));
    cx.run_until_parked();
    view.update_in(cx, |w, window, cx| w.save_search(window, cx));
    assert!(!fill_and_submit(&view, "mine", None, cx));
    view.read_with(cx, |w, cx| {
        let dialog = w.saved_search_dialog().expect("still open").read(cx);
        assert!(
            dialog.error().is_some_and(|e| e.contains("exists already")),
            "{:?}",
            dialog.error()
        );
    });
    assert_eq!(saved_names(&fake), ["Mine"]);

    // Selecting it in the pane later opens it like a folder.
    view.update_in(cx, |w, window, cx| {
        let pane = w.folder_pane.clone();
        let selected = FolderPaneEvent::Selected {
            key: key.clone(),
            title: "Mine".into(),
        };
        w.on_folder_pane_event(&pane, &selected, window, cx);
    });
    cx.run_until_parked();
    assert_eq!(scope(&view, cx), ThreadScope::Folder(key));
    assert_eq!(list_len(&view, cx), found);
}

#[gpui::test]
fn a_saved_search_is_edited_and_deleted_from_its_menu(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_with_root(cx);
    let word = some_word(&view, cx);
    let id: SavedSearchId = fake.save_search("Mine", &word).unwrap();
    let key = FolderKey::SavedSearch(id);
    view.update_in(cx, |w, window, cx| {
        w.folder_pane.update(cx, |pane, cx| pane.reload(cx));
        let pane = w.folder_pane.clone();
        let selected = FolderPaneEvent::Selected {
            key: key.clone(),
            title: "Mine".into(),
        };
        w.on_folder_pane_event(&pane, &selected, window, cx);
    });
    cx.run_until_parked();
    assert!(list_len(&view, cx) > 0);

    // Edit: a new name and a query that finds nothing.
    view.update_in(cx, |w, window, cx| {
        let pane = w.folder_pane.clone();
        w.on_folder_pane_event(&pane, &FolderPaneEvent::EditSavedSearch { id }, window, cx);
    });
    let (name, query) = view.read_with(cx, |w, cx| {
        let dialog = w.saved_search_dialog().expect("the edit dialog").read(cx);
        assert_eq!(dialog.editing(), Some(id));
        (
            dialog.name.read(cx).value().to_string(),
            dialog.query.read(cx).value().to_string(),
        )
    });
    assert_eq!((name.as_str(), query.as_str()), ("Mine", word.as_str()));
    assert!(fill_and_submit(&view, "Nothing", Some("zzqxnothing"), cx));
    assert_eq!(saved_names(&fake), ["Nothing"]);
    assert_eq!(scope(&view, cx), ThreadScope::Folder(key.clone()));
    assert_eq!(
        title(&view, cx),
        "Nothing",
        "the open view follows the name"
    );
    assert_eq!(list_len(&view, cx), 0, "and the query");

    // Delete asks first; the view goes back to the unified Inbox.
    view.update_in(cx, |w, window, cx| {
        let pane = w.folder_pane.clone();
        w.on_folder_pane_event(
            &pane,
            &FolderPaneEvent::DeleteSavedSearch { id },
            window,
            cx,
        );
    });
    cx.run_until_parked();
    assert!(cx.has_pending_prompt(), "asks before deleting");
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(saved_names(&fake), ["Nothing"]);
    view.update_in(cx, |w, window, cx| w.delete_saved_search(id, window, cx));
    cx.run_until_parked();
    cx.simulate_prompt_answer("Delete");
    cx.run_until_parked();
    assert!(saved_names(&fake).is_empty());
    assert_eq!(
        scope(&view, cx),
        ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox))
    );
    view.read_with(cx, |w, cx| {
        assert_eq!(w.folder_pane().read(cx).title_of(&key), None);
    });
}

#[gpui::test]
fn saved_searches_change_from_outside_reach_the_open_view(cx: &mut TestAppContext) {
    // `mazemail saved-search rename|edit` change the store; the window hears it as a
    // folder change.
    let (view, fake, cx) = open_with_root(cx);
    let word = some_word(&view, cx);
    let id = fake.save_search("Mine", &word).unwrap();
    let key = FolderKey::SavedSearch(id);
    let changes = DataChanges {
        folders: true,
        ..DataChanges::default()
    };
    view.update_in(cx, |w, window, cx| {
        w.apply_changes(&changes, window, cx);
        let pane = w.folder_pane.clone();
        let selected = FolderPaneEvent::Selected {
            key: key.clone(),
            title: "Mine".into(),
        };
        w.on_folder_pane_event(&pane, &selected, window, cx);
    });
    cx.run_until_parked();
    fake.edit_saved_search(id, "Elsewhere", "zzqxnothing")
        .unwrap();
    view.update_in(cx, |w, window, cx| w.apply_changes(&changes, window, cx));
    cx.run_until_parked();
    assert_eq!(title(&view, cx), "Elsewhere");
    assert_eq!(list_len(&view, cx), 0);
}

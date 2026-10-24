//! The settings tab: `settings.json` in an editor, saved with `cmd-s` and applied at
//! once, with diagnostics, changes on disk and the close prompt.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ::settings::{DiagnosticKind, Layout, SettingsWatcher};
use gpui::{Modifiers, TestAppContext, VisualTestContext};
use time::macros::datetime;
use ui::panes::SerializedMember;

use super::*;
use crate::actions::{OpenDefaultSettings, OpenSettings};
use crate::fake::FakeMailSource;
use crate::settings_editor::DISK_CHECK_INTERVAL;

/// A main window editing `<tmp>/config/settings.json` (holding `initial`, if any), its
/// saves reloading a real settings watcher.
fn open_with_settings<'a>(
    cx: &'a mut TestAppContext,
    initial: Option<&str>,
) -> (
    Entity<MailWindow>,
    &'a mut VisualTestContext,
    tempfile::TempDir,
    PathBuf,
    Rc<SettingsWatcher>,
) {
    cx.update(|cx| {
        ui::init(cx);
        crate::init(cx);
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if let Some(text) = initial {
        std::fs::write(&path, text).unwrap();
    }
    // A long debounce: only the editor's reload delivers changes during the test.
    let (watcher, _) = SettingsWatcher::start(&path, Duration::from_secs(600)).unwrap();
    let watcher = Rc::new(watcher);
    let reload = watcher.clone();
    let options = MailWindowOptions {
        settings_file: Some(SettingsFile::new(&path).on_save(move |_| reload.reload())),
        ..MailWindowOptions::default()
    };
    let source: Arc<dyn MailSource> =
        Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
    let (view, cx) = cx.add_window_view(|window, cx| MailWindow::new(source, &options, window, cx));
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    (view, cx, dir, path, watcher)
}

fn settings_tab(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> Entity<SettingsEditor> {
    view.read_with(cx, |w, cx| {
        w.panes()
            .read(cx)
            .items_of::<SettingsEditor>(cx)
            .into_iter()
            .find(|tab| !tab.read(cx).is_defaults())
            .expect("a settings tab")
    })
}

fn text(tab: &Entity<SettingsEditor>, cx: &mut VisualTestContext) -> String {
    tab.read_with(cx, |tab, cx| tab.text(cx))
}

/// Replaces the buffer as typing would (an edit the tab sees).
fn type_text(tab: &Entity<SettingsEditor>, text: &str, cx: &mut VisualTestContext) {
    let editor = tab.read_with(cx, |tab, _| tab.editor().clone());
    editor.update_in(cx, |editor, window, cx| {
        editor.replace_all(text, window, cx)
    });
    cx.run_until_parked();
}

fn active_is_dirty(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> bool {
    view.read_with(cx, |w, cx| {
        w.panes().read(cx).active_item(cx).unwrap().is_dirty(cx)
    })
}

fn applied_layout(watcher: &SettingsWatcher) -> Layout {
    ::settings::load_settings(watcher.path()).settings.layout
}

fn disk(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[gpui::test]
fn cmd_comma_opens_settings_json_and_creates_it(cx: &mut TestAppContext) {
    let (view, cx, _dir, path, _watcher) = open_with_settings(cx, None);
    assert!(!path.exists());
    cx.simulate_keystrokes("cmd-,");
    cx.run_until_parked();
    let tab = settings_tab(&view, cx);
    assert_eq!(disk(&path), ::settings::USER_SETTINGS_TEMPLATE);
    assert_eq!(text(&tab, cx), ::settings::USER_SETTINGS_TEMPLATE);
    assert!(tab.read_with(cx, |t, _| t.diagnostics().is_empty()));
    let (title, active) = view.read_with(cx, |w, cx| {
        let item = w.panes().read(cx).active_item(cx).unwrap();
        (
            item.tab_title(cx).to_string(),
            item.downcast::<SettingsEditor>(),
        )
    });
    assert_eq!(title, "Settings");
    assert_eq!(active, Some(tab.clone()));
    // The editor has the focus: typing goes into the file's buffer.
    cx.simulate_input("x");
    assert!(text(&tab, cx).contains('x'));

    // Again: the open tab, not a second one.
    cx.dispatch_action(OpenSettings);
    cx.run_until_parked();
    let count = view.read_with(cx, |w, cx| {
        w.panes().read(cx).items_of::<SettingsEditor>(cx).len()
    });
    assert_eq!(count, 1);

    // The banner explains the file and links to the defaults, read-only.
    assert!(cx.debug_bounds("settings-banner").is_some());
    let link = cx
        .debug_bounds("settings-open-defaults")
        .expect("the link to the defaults");
    cx.simulate_click(link.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    let defaults = view
        .read_with(cx, |w, cx| {
            w.panes()
                .read(cx)
                .active_item(cx)
                .unwrap()
                .downcast::<SettingsEditor>()
        })
        .unwrap();
    assert!(defaults.read_with(cx, |d, _| d.is_defaults()));
    assert_eq!(text(&defaults, cx), ::settings::DEFAULT_SETTINGS);
    assert_eq!(
        view.read_with(cx, |w, cx| {
            w.panes()
                .read(cx)
                .active_item(cx)
                .unwrap()
                .tab_title(cx)
                .to_string()
        }),
        "Default Settings"
    );
    cx.simulate_input("y");
    assert_eq!(text(&defaults, cx), ::settings::DEFAULT_SETTINGS);
    assert!(!active_is_dirty(&view, cx));
}

#[gpui::test]
fn cmd_s_saves_and_applies_the_settings(cx: &mut TestAppContext) {
    let (view, cx, _dir, path, watcher) = open_with_settings(cx, Some("{}\n"));
    cx.simulate_keystrokes("cmd-,");
    cx.run_until_parked();
    let tab = settings_tab(&view, cx);
    type_text(&tab, "{\n  // Wide.\n  \"layout\": \"wide\"\n}", cx);
    assert!(active_is_dirty(&view, cx), "the tab's dot");
    assert_eq!(disk(&path), "{}\n", "not saved yet");

    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    assert_eq!(disk(&path), "{\n  // Wide.\n  \"layout\": \"wide\"\n}\n");
    assert!(!active_is_dirty(&view, cx));
    assert!(tab.read_with(cx, |t, _| t.diagnostics().is_empty()));
    assert_eq!(applied_layout(&watcher), Layout::Wide);
}

#[gpui::test]
fn saving_reloads_the_watcher_at_once(cx: &mut TestAppContext) {
    cx.update(|cx| {
        ui::init(cx);
        crate::init(cx);
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, "{}\n").unwrap();
    let (watcher, mut receiver) = SettingsWatcher::start(&path, Duration::from_secs(600)).unwrap();
    receiver.borrow_and_update();
    let options = MailWindowOptions {
        settings_file: Some(SettingsFile::new(&path).on_save(move |_| watcher.reload())),
        ..MailWindowOptions::default()
    };
    let source: Arc<dyn MailSource> =
        Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
    let (view, cx) = cx.add_window_view(|window, cx| MailWindow::new(source, &options, window, cx));
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    let tab = view
        .update_in(cx, |w, window, cx| w.open_settings(false, window, cx))
        .unwrap();
    cx.run_until_parked();
    type_text(&tab, r#"{ "layout": "classic" }"#, cx);
    assert!(!receiver.has_changed().unwrap());
    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    // Published by the save, without waiting for the file system.
    assert!(receiver.has_changed().unwrap());
    assert_eq!(
        receiver.borrow_and_update().settings.layout,
        Layout::Classic
    );

    // A syntax error: diagnostics with the line, the last good settings stay.
    type_text(&tab, "{\n  \"layout\": \"wide\",\n  \"theme\": \n}", cx);
    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    let diagnostics = tab.read_with(cx, |t, _| t.diagnostics().to_vec());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].kind, DiagnosticKind::Syntax);
    assert_eq!(diagnostics[0].line, Some(4));
    assert!(cx.debug_bounds("settings-diagnostics").is_some());
    let loaded = receiver.borrow_and_update().clone();
    assert_eq!(
        loaded.settings.layout,
        Layout::Classic,
        "the last good settings"
    );
    assert!(loaded.file_ignored());

    // Unknown keys and wrong values: listed with their lines, the rest applies.
    type_text(
        &tab,
        "{\n  \"layout\": \"wide\",\n  \"lay_out\": 1,\n  \"agent_panel\": { \"width\": \"wide\" }\n}",
        cx,
    );
    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    let lines: Vec<_> = tab.read_with(cx, |t, _| {
        t.diagnostics()
            .iter()
            .map(|d| (d.path.clone(), d.line, d.is_error()))
            .collect()
    });
    assert_eq!(
        lines,
        vec![
            ("agent_panel.width".to_owned(), Some(4), true),
            ("lay_out".to_owned(), Some(3), false),
        ]
    );
    assert_eq!(receiver.borrow_and_update().settings.layout, Layout::Wide);

    // Fixed: the strip goes away.
    type_text(&tab, r#"{ "layout": "wide" }"#, cx);
    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    assert!(tab.read_with(cx, |t, _| t.diagnostics().is_empty()));
    assert!(cx.debug_bounds("settings-diagnostics").is_none());
}

#[gpui::test]
fn changes_on_disk_reload_a_clean_buffer_and_ask_about_a_dirty_one(cx: &mut TestAppContext) {
    let (view, cx, _dir, path, _watcher) = open_with_settings(cx, Some("{}\n"));
    let tab = view
        .update_in(cx, |w, window, cx| w.open_settings(false, window, cx))
        .unwrap();
    cx.run_until_parked();

    // Maze writes the favorite folders into the file itself.
    ::settings::write_setting(
        &path,
        &["folder_pane", "favorites"],
        &serde_json::json!([{ "account": "work", "folder": "Acme" }]),
    )
    .unwrap();
    cx.executor().advance_clock(DISK_CHECK_INTERVAL);
    cx.run_until_parked();
    assert_eq!(
        text(&tab, cx),
        disk(&path),
        "a clean buffer follows the file"
    );
    assert!(text(&tab, cx).contains("Acme"));
    assert!(!active_is_dirty(&view, cx));
    assert!(!tab.read_with(cx, |t, _| t.has_conflict()));

    // With unsaved edits: ask, keep the edits meanwhile.
    type_text(&tab, r#"{ "layout": "wide" }"#, cx);
    std::fs::write(&path, r#"{ "layout": "classic" }"#).unwrap();
    cx.executor().advance_clock(DISK_CHECK_INTERVAL);
    cx.run_until_parked();
    assert!(tab.read_with(cx, |t, _| t.has_conflict()));
    assert!(cx.debug_bounds("settings-conflict").is_some());
    assert_eq!(text(&tab, cx), r#"{ "layout": "wide" }"#);
    // Asked once, not again every second.
    cx.executor().advance_clock(DISK_CHECK_INTERVAL * 3);
    cx.run_until_parked();
    assert!(tab.read_with(cx, |t, _| t.has_conflict()));

    // "Keep Mine": the edits stay, saving replaces the file.
    let keep_mine = cx.debug_bounds("settings-keep-mine").expect("Keep Mine");
    cx.simulate_click(keep_mine.center(), Modifiers::none());
    cx.run_until_parked();
    assert!(!tab.read_with(cx, |t, _| t.has_conflict()));
    assert!(active_is_dirty(&view, cx));
    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    assert_eq!(disk(&path), "{ \"layout\": \"wide\" }\n");
    cx.executor().advance_clock(DISK_CHECK_INTERVAL);
    cx.run_until_parked();
    assert!(!tab.read_with(cx, |t, _| t.has_conflict()), "our own save");

    // "Reload": the file's text, the edits are gone.
    type_text(&tab, "{ \"layout\": \"four_column\" }", cx);
    std::fs::write(&path, r#"{ "layout": "vertical" }"#).unwrap();
    cx.executor().advance_clock(DISK_CHECK_INTERVAL);
    cx.run_until_parked();
    assert!(tab.read_with(cx, |t, _| t.has_conflict()));
    let reload = cx.debug_bounds("settings-reload").expect("Reload");
    cx.simulate_click(reload.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(text(&tab, cx), r#"{ "layout": "vertical" }"#);
    assert!(!active_is_dirty(&view, cx));
    assert!(cx.debug_bounds("settings-conflict").is_none());
}

#[gpui::test]
fn closing_with_unsaved_changes_asks_first(cx: &mut TestAppContext) {
    let (view, cx, _dir, path, watcher) = open_with_settings(cx, Some("{}\n"));
    cx.simulate_keystrokes("cmd-,");
    cx.run_until_parked();
    let tab = settings_tab(&view, cx);
    type_text(&tab, r#"{ "layout": "classic" }"#, cx);
    let settings_tabs = |cx: &mut VisualTestContext| {
        view.read_with(cx, |w, cx| {
            w.panes().read(cx).items_of::<SettingsEditor>(cx).len()
        })
    };

    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(settings_tabs(cx), 1, "kept");
    assert_eq!(disk(&path), "{}\n");

    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Save");
    cx.run_until_parked();
    assert_eq!(settings_tabs(cx), 0, "saved, then closed");
    assert_eq!(disk(&path), "{ \"layout\": \"classic\" }\n");
    assert_eq!(applied_layout(&watcher), Layout::Classic);

    // "Don't Save" closes and leaves the file alone; a clean tab closes at once.
    cx.simulate_keystrokes("cmd-,");
    cx.run_until_parked();
    let tab = settings_tab(&view, cx);
    type_text(&tab, r#"{ "layout": "wide" }"#, cx);
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    assert_eq!(settings_tabs(cx), 0);
    assert_eq!(disk(&path), "{ \"layout\": \"classic\" }\n");
    cx.simulate_keystrokes("cmd-,");
    cx.run_until_parked();
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(settings_tabs(cx), 0);
}

#[gpui::test]
fn settings_tabs_come_back_with_the_layout(cx: &mut TestAppContext) {
    let (view, vcx, _dir, path, _watcher) = open_with_settings(cx, Some("{}\n"));
    vcx.dispatch_action(OpenDefaultSettings);
    vcx.run_until_parked();
    vcx.simulate_keystrokes("cmd-,");
    vcx.run_until_parked();
    let layout = view.update(vcx, |w, cx| {
        w.save_layout(cx);
        w.panes().read(cx).serialize(cx)
    });
    let SerializedMember::Pane(pane) = &layout.root else {
        panic!("one pane");
    };
    let kinds: Vec<_> = pane
        .items
        .iter()
        .map(|item| (item.kind.as_str(), item.data.clone()))
        .collect();
    assert_eq!(
        kinds[1..],
        [
            ("settings", serde_json::json!({ "defaults": true })),
            ("settings", serde_json::json!({})),
        ]
    );

    let ui_state = UiStateStore::in_memory(ListDefaults::default());
    ui_state.set_layout(layout);
    let options = MailWindowOptions {
        ui_state,
        settings_file: Some(SettingsFile::new(&path)),
        ..MailWindowOptions::default()
    };
    let source: Arc<dyn MailSource> =
        Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
    let (reopened, rcx) =
        cx.add_window_view(|window, cx| MailWindow::new(source, &options, window, cx));
    rcx.run_until_parked();
    let titles: Vec<String> = reopened.read_with(rcx, |w, cx| {
        w.panes()
            .read(cx)
            .items(cx)
            .iter()
            .map(|item| item.tab_title(cx).to_string())
            .collect()
    });
    assert_eq!(titles[1..], ["Default Settings", "Settings"]);
    // The active tab (the settings) is restored at once.
    let tab = reopened.read_with(rcx, |w, cx| {
        w.panes().read(cx).items_of::<SettingsEditor>(cx)
    });
    assert_eq!(tab.len(), 1);
    assert!(!tab[0].read_with(rcx, |t, _| t.is_defaults()));
    // cmd-, finds it; the defaults are restored when asked for.
    rcx.update(|window, _| window.activate_window());
    reopened.update_in(rcx, |w, window, cx| w.open_settings(true, window, cx));
    rcx.run_until_parked();
    let count = reopened.read_with(rcx, |w, cx| {
        w.panes().read(cx).items_of::<SettingsEditor>(cx).len()
    });
    assert_eq!(count, 2);
}

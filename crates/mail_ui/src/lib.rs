//! The Maze main window (layer L3), CONCEPT §8.2 and ADR-018:
//!
//! ```text
//! spaces | folder pane | center: tabs and splits | agent column
//! ```
//!
//! The center is a `ui::panes::PaneGroup` of mail views (message list and message),
//! message tabs, drafts and the settings ([`settings_editor`]).
//!
//! The UI reads mail through the [`MailSource`] trait, so it doesn't depend on any
//! non-UI Maze crate except `mazemail_types` and `settings` (the settings tab parses
//! and writes `settings.json` itself). The `mazemail` binary implements it
//! over the core (`CoreMailSource`) and feeds the core's change events in through
//! [`MailWindow::apply_changes`]; [`FakeMailSource`] provides synthetic data
//! (including a 1,000,000-thread folder) for demos (`MAZEMAIL_DEMO=1`), tests and
//! benchmarks.
//!
//! Start-up: [`ui::init`], then [`init`], then [`open_main_window`].

pub mod actions;
mod chrome;
pub mod command_palette;
pub mod fake;
pub mod folder_pane;
pub mod format;
pub mod layout;
pub mod links;
pub mod list_model;
pub mod mail_view;
pub mod message_tab;
pub mod message_view;
pub mod perf;
pub mod quick_move;
pub mod selection;
pub mod settings_editor;
pub mod source;
pub mod sync_activity;
pub mod thread_list;
pub mod ui_state;
pub mod window;

use gpui::{App, Menu, MenuItem, OsAction, SystemMenuType};
use gpui_kit::component::input;
use mazemail_types::PRODUCT_NAME;

pub use fake::FakeMailSource;
pub use settings_editor::{SettingsEditor, SettingsFile};
pub use source::{DataChanges, MailSource};
pub use ui_state::{ListDefaults, ListMode, UI_STATE_FILE, UiStateStore};
pub use window::{
    ComposeRequest, MailWindow, MailWindowOptions, OpenWindowError, open_main_window,
};

use crate::actions::{
    About, CollapseAllThreads, ExpandAllThreads, Forward, Hide, HideOthers, Minimize, NewMessage,
    OpenSettings, Quit, Reply, ReplyAll, ReverseSortOrder, ShowAll, SyncNow, Toggle,
    ToggleAgentColumn, ToggleFolderPane, ToggleMessagePane, ToggleTableView, ToggleThreads, Zoom,
    sort_action,
};
use crate::source::SortBy;

/// Registers application-wide action handlers, binds the default keymap and sets the
/// menu bar. Call after [`ui::init`].
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());

    match ui::keymap::bind_keymap(ui::keymap::DEFAULT_MACOS_KEYMAP, cx) {
        Ok(errors) if errors.is_empty() => {}
        Ok(errors) => tracing::warn!(count = errors.len(), "default keymap has invalid bindings"),
        Err(error) => tracing::error!(%error, "default keymap failed to load"),
    }
    cx.set_menus(app_menus());
}

/// The menu bar. On macOS the first menu is the application menu; its title comes
/// from the bundle (`Maze.app`), the items say "Maze".
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new(PRODUCT_NAME).items([
            MenuItem::action(format!("About {PRODUCT_NAME}"), About),
            MenuItem::separator(),
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action(format!("Hide {PRODUCT_NAME}"), Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action(format!("Quit {PRODUCT_NAME}"), Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", input::Paste, OsAction::Paste),
            MenuItem::os_action("Select All", input::SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", Toggle),
            MenuItem::separator(),
            MenuItem::action("Folder Pane", ToggleFolderPane),
            MenuItem::action("Message Pane", ToggleMessagePane),
            MenuItem::action("Agent Column", ToggleAgentColumn),
            MenuItem::separator(),
            MenuItem::action("Table View", ToggleTableView),
            MenuItem::action("Threads", ToggleThreads),
            MenuItem::action("Expand All Threads", ExpandAllThreads),
            MenuItem::action("Collapse All Threads", CollapseAllThreads),
            MenuItem::separator(),
            MenuItem::submenu(
                Menu::new("Sort By").items(
                    SortBy::ALL
                        .into_iter()
                        .map(|by| MenuItem::Action {
                            name: by.label().into(),
                            action: sort_action(by),
                            os_action: None,
                            checked: false,
                            disabled: false,
                        })
                        .chain([
                            MenuItem::separator(),
                            MenuItem::action("Reverse Sort Order", ReverseSortOrder),
                        ]),
                ),
            ),
        ]),
        Menu::new("Message").items([
            MenuItem::action("New Message", NewMessage),
            MenuItem::action("Sync Now", SyncNow),
            MenuItem::separator(),
            MenuItem::action("Reply", Reply),
            MenuItem::action("Reply All", ReplyAll),
            MenuItem::action("Forward", Forward),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
    ]
}

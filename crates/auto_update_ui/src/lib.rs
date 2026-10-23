//! Maze's auto-update UI (layer L3, PLAN §4.15), on top of `auto_update`.
//!
//! - [`AutoUpdater`]: a GPUI entity around [`auto_update::Updater`]. It mirrors the
//!   updater's status (a `tokio::sync::watch` channel), polls hourly and after wake-up
//!   while `auto_update` is on, applies `auto_update`, `update_channel` and
//!   `auto_update_install` live ([`AutoUpdater::follow_settings`]), and restarts into
//!   the new version (`cx.restart()`, asking a [`RestartGuard`] first). Its Tokio work
//!   runs through `ui::gpui_tokio::spawn_on`.
//! - [`UpdateButton`]: the title bar element. Nothing when idle, a small progress
//!   indicator while downloading, **"Restart to update"** once installed, "Update to
//!   x.y.z" in `notify_only` mode, and a warning icon with the error in its tooltip.
//!   What it shows is the pure function [`title_bar_state`].
//! - [`CheckForUpdates`] (`maze::CheckForUpdates`, "Maze: Check for Updates" in the
//!   command palette and the app menu). Builds that don't update (dev builds, builds
//!   without update keys, distribution packages) answer with the reason instead.
//! - [`show_update_notice`]: the "Updated to x.y.z" toast with a link to the release
//!   notes after the restart.
//!
//! The `mazemail` binary creates the updater at startup (not for `--headless` or the
//! CLI), puts the button into the main window's title bar and calls [`init`].

mod settings;
mod state;
mod updater;
mod view;

use auto_update::{CheckOutcome, UpdateError, UpdateNotice};
use gpui::{App, Menu, MenuItem, actions};
use gpui_kit::component::{
    Sizable as _, WindowExt as _, button::Button, notification::Notification,
};
use mazemail_types::PRODUCT_NAME;

pub use crate::settings::UpdateSettings;
pub use crate::state::{StateContext, TitleBarState, title_bar_state};
pub use crate::updater::{AutoUpdater, AutoUpdaterOptions, RestartGuard};
pub use crate::view::UpdateButton;

actions!(
    maze,
    [
        /// Check for a new version of Maze now.
        CheckForUpdates,
    ]
);

/// The one-line notice for installs Maze can't update in place (PLAN §4.15).
pub const MOVE_TO_APPLICATIONS: &str = "Move Maze to Applications to get automatic updates.";

/// The app menu item's label.
pub const MENU_LABEL: &str = "Check for Updates…";

/// Registers the [`CheckForUpdates`] handler. Call once at startup.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &CheckForUpdates, cx| check_for_updates(cx));
}

/// Adds "Check for Updates…" after "About Maze" in the app menu (the first menu).
pub fn add_menu_item(menus: &mut [Menu]) {
    if let Some(app_menu) = menus.first_mut() {
        let at = app_menu.items.len().min(1);
        app_menu
            .items
            .insert(at, MenuItem::action(MENU_LABEL, CheckForUpdates));
    }
}

/// Shows a notification in the active (or else the first) window.
pub(crate) fn notify(cx: &mut App, note: Notification) {
    let window = cx.active_window().or_else(|| cx.windows().first().copied());
    match window {
        Some(window) => {
            if let Err(error) =
                window.update(cx, |_, window, cx| window.push_notification(note, cx))
            {
                tracing::debug!(%error, "cannot show a notification");
            }
        }
        None => tracing::debug!("no window for a notification"),
    }
}

/// "Maze: Check for Updates": checks now and reports the result, or explains why this
/// build doesn't update.
pub fn check_for_updates(cx: &mut App) {
    let Some(updater) = AutoUpdater::global(cx) else {
        notify(
            cx,
            Notification::info(format!("{PRODUCT_NAME} can't check for updates yet."))
                .title(PRODUCT_NAME),
        );
        return;
    };
    if let Some(reason) = updater.read(cx).disabled_reason() {
        tracing::info!(%reason, "updates are disabled");
        notify(
            cx,
            Notification::info(disabled_message(&reason)).title(PRODUCT_NAME),
        );
        return;
    }
    let task = updater.update(cx, |updater, cx| updater.check(cx));
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            if let Some(note) = check_result_notification(&result) {
                notify(cx, note);
            }
        });
    })
    .detach();
}

/// The answer of "Check for Updates" in a build that doesn't update.
pub fn disabled_message(reason: &str) -> String {
    let reason = reason.trim_end_matches('.');
    format!("Updates are off: {reason}.")
}

/// The answer of "Check for Updates" once the check finished. The title bar shows the
/// rest (download progress, "Restart to update").
fn check_result_notification(
    result: &Result<CheckOutcome, std::sync::Arc<UpdateError>>,
) -> Option<Notification> {
    let note = match result {
        Ok(CheckOutcome::UpToDate) => Notification::success(format!(
            "{PRODUCT_NAME} is up to date ({}).",
            release_channel::VERSION
        )),
        Ok(CheckOutcome::Available(version)) => {
            Notification::info(format!("{PRODUCT_NAME} {version} is available."))
        }
        Ok(CheckOutcome::Installed(version)) => Notification::success(format!(
            "{PRODUCT_NAME} {version} is installed. Restart to update."
        )),
        Ok(CheckOutcome::Busy) => return None,
        Err(error) => {
            tracing::warn!(%error, "update check failed");
            Notification::error(format!("Update failed: {error}"))
        }
    };
    Some(note.title(PRODUCT_NAME))
}

/// "Updated to x.y.z" with a "Release notes" button, after the first launch of a new
/// version.
pub fn show_update_notice(notice: &UpdateNotice, cx: &mut App) {
    let mut note =
        Notification::success(format!("Updated to {}", notice.version)).title(PRODUCT_NAME);
    if let Some(url) = notice.notes_url.clone() {
        note = note.action(move |_, _, _| {
            let url = url.clone();
            Button::new("release-notes")
                .small()
                .label("Release notes")
                .on_click(move |_, _, cx| cx.open_url(url.as_str()))
        });
    }
    notify(cx, note);
}

#[cfg(test)]
mod tests;

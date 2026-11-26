//! The settings the message views follow: reader or original mode for HTML mail in the
//! message pane (`reader.default_mode`) and in message tabs (`reader.tab_mode`,
//! ADR-020), and the folder the save dialog of an attachment starts in
//! (`download_dir`).
//!
//! Applied from the settings file when the app starts, and then whenever the core's
//! settings change, like the theme.

use gpui::App;
use mail_ui::message_view::{BodyModes, set_body_modes, set_download_dir};
use mazemail_core::CoreHandle;
use settings::{Paths, Settings};

/// Applies the settings file on disk (at startup).
pub(crate) fn apply_from_file(paths: &Paths, cx: &mut App) {
    let loaded = settings::load_settings(&paths.settings_file());
    apply(&loaded.settings, cx);
}

/// Applies the settings whenever the core's settings change.
pub(crate) fn follow_core_settings(core: &CoreHandle, cx: &mut App) {
    let mut settings = core.settings_watch();
    cx.spawn(async move |cx| {
        loop {
            let current = settings.borrow_and_update().settings.clone();
            cx.update(|cx| apply(&current, cx));
            if settings.changed().await.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn apply(settings: &Settings, cx: &mut App) {
    set_body_modes(BodyModes::from_settings(&settings.reader), cx);
    let home = std::env::home_dir();
    set_download_dir(settings.download_dir_path(home.as_deref()), cx);
}

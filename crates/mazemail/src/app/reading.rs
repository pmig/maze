//! The `reader` settings the message views follow: reader or original mode for HTML
//! mail in the message pane (`default_mode`) and in message tabs (`tab_mode`,
//! ADR-020).
//!
//! Applied from the settings file when the app starts, and then whenever the core's
//! settings change, like the theme.

use gpui::App;
use mail_ui::message_view::{BodyModes, set_body_modes};
use mazemail_core::CoreHandle;
use settings::Paths;

/// Applies the modes from the settings file on disk (at startup).
pub(crate) fn apply_from_file(paths: &Paths, cx: &mut App) {
    let loaded = settings::load_settings(&paths.settings_file());
    set_body_modes(BodyModes::from_settings(&loaded.settings.reader), cx);
}

/// Applies the modes whenever the core's settings change.
pub(crate) fn follow_core_settings(core: &CoreHandle, cx: &mut App) {
    let mut settings = core.settings_watch();
    cx.spawn(async move |cx| {
        loop {
            let modes = BodyModes::from_settings(&settings.borrow_and_update().settings.reader);
            cx.update(|cx| set_body_modes(modes, cx));
            if settings.changed().await.is_err() {
                break;
            }
        }
    })
    .detach();
}

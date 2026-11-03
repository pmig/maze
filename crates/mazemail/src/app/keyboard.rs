//! The `keymap` settings. `primary_modifier`: which key the shortcuts written with `cmd`
//! use (Command on macOS and Ctrl on Linux by default, or either on both). Read from
//! the settings file when the app starts, before the default keymap is bound; GPUI
//! can't take single bindings back, so a change applies after a restart.
//! `show_on_buttons`: the shortcuts on the buttons above a message, applied at once.

use gpui::App;
use mail_ui::message_view::set_button_shortcuts;
use mazemail_core::CoreHandle;
use settings::{Paths, PrimaryModifier};
use ui::keymap::{self, PrimaryModifier as Modifier};

/// The modifier the setting names.
pub(crate) fn modifier(setting: PrimaryModifier) -> Modifier {
    match setting {
        PrimaryModifier::Platform => Modifier::for_platform(),
        PrimaryModifier::Ctrl => Modifier::Ctrl,
        PrimaryModifier::Super => Modifier::Super,
    }
}

/// Applies the setting from the settings file on disk. Call before `mail_ui::init`.
pub(crate) fn apply_from_file(paths: &Paths, cx: &mut App) {
    let loaded = settings::load_settings(&paths.settings_file());
    keymap::set_primary_modifier(modifier(loaded.settings.keymap.primary_modifier), cx);
    set_button_shortcuts(loaded.settings.keymap.show_on_buttons, cx);
}

/// Shows or hides the shortcuts on buttons whenever the core's settings change.
pub(crate) fn follow_core_settings(core: &CoreHandle, cx: &mut App) {
    let mut settings = core.settings_watch();
    cx.spawn(async move |cx| {
        loop {
            let show = settings.borrow_and_update().settings.keymap.show_on_buttons;
            cx.update(|cx| set_button_shortcuts(show, cx));
            if settings.changed().await.is_err() {
                break;
            }
        }
    })
    .detach();
}

/// Applies the platform's convention (demo mode, without a settings file).
pub(crate) fn apply_platform(cx: &mut App) {
    keymap::set_primary_modifier(Modifier::for_platform(), cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_picks_the_modifier() {
        assert_eq!(modifier(PrimaryModifier::Ctrl), Modifier::Ctrl);
        assert_eq!(modifier(PrimaryModifier::Super), Modifier::Super);
        let expected = if cfg!(target_os = "macos") {
            Modifier::Super
        } else {
            Modifier::Ctrl
        };
        assert_eq!(modifier(PrimaryModifier::Platform), expected);
    }
}

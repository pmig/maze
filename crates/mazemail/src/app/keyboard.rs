//! The `keymap.primary_modifier` setting: which key the shortcuts written with `cmd`
//! use (Command on macOS and Ctrl on Linux by default, or either on both).
//!
//! Read from the settings file when the app starts, before the default keymap is bound.
//! GPUI can't take single bindings back, so a change applies after a restart.

use gpui::App;
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

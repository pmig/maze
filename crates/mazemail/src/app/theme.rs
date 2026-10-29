//! The `theme` setting: light or dark (or the system's), and the theme for each, from
//! the bundled themes and `<config>/themes/*.json` (Zed's format). The `font_size`
//! setting goes with it, as the base size of the theme.
//!
//! Applied from the settings file when the app starts, before the first window, and
//! then whenever the core's settings change (a save in the settings tab, an edit in
//! another editor).

use std::path::PathBuf;

use gpui::App;
use mazemail_core::CoreHandle;
use settings::{Paths, Settings, ThemeMode};
use ui::theme::{Appearance, ThemeChoice, apply_choice, clamp_font_size};

/// The theme choice the settings describe.
pub(crate) fn choice(settings: &Settings, themes_dir: Option<PathBuf>) -> ThemeChoice {
    ThemeChoice {
        appearance: match settings.theme.mode {
            ThemeMode::System => Appearance::System,
            ThemeMode::Light => Appearance::Light,
            ThemeMode::Dark => Appearance::Dark,
        },
        light: settings.theme.light.clone(),
        dark: settings.theme.dark.clone(),
        themes_dir,
        font_size: clamp_font_size(settings.font_size as f32),
    }
}

/// Applies the theme from the settings file on disk (at startup, before the core is
/// open).
pub(crate) fn apply_from_file(paths: &Paths, cx: &mut App) {
    let loaded = settings::load_settings(&paths.settings_file());
    apply_choice(choice(&loaded.settings, Some(paths.themes_dir())), cx);
}

/// Applies the theme whenever the core's settings change.
pub(crate) fn follow_core_settings(core: &CoreHandle, cx: &mut App) {
    let themes_dir = Paths::current().ok().map(|paths| paths.themes_dir());
    let mut settings = core.settings_watch();
    cx.spawn(async move |cx| {
        loop {
            let choice = choice(&settings.borrow_and_update().settings, themes_dir.clone());
            cx.update(|cx| apply_choice(choice, cx));
            if settings.changed().await.is_err() {
                break;
            }
        }
    })
    .detach();
}

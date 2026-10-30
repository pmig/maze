//! The `spellcheck` setting the composer follows: the languages to check drafts in,
//! the system's preferred languages when it names none, or off.
//!
//! Applied from the settings file when the app starts, and then whenever the core's
//! settings change, like the theme; open drafts re-check at once.

use gpui::App;
use mazemail_core::CoreHandle;
use settings::{Paths, Spellcheck};

/// Applies the setting from the settings file on disk (at startup).
pub(crate) fn apply_from_file(paths: &Paths, cx: &mut App) {
    let loaded = settings::load_settings(&paths.settings_file());
    apply(&loaded.settings.spellcheck, cx);
}

/// Applies the setting whenever the core's settings change.
pub(crate) fn follow_core_settings(core: &CoreHandle, cx: &mut App) {
    let mut settings = core.settings_watch();
    cx.spawn(async move |cx| {
        loop {
            let spellcheck = settings.borrow_and_update().settings.spellcheck.clone();
            cx.update(|cx| apply(&spellcheck, cx));
            if settings.changed().await.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn apply(spellcheck: &Spellcheck, cx: &mut App) {
    let languages = spellcheck.resolve(sys_locale::get_locales());
    tracing::debug!(?languages, "spell checking languages");
    composer::set_spellcheck(languages, cx);
}

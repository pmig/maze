//! Actions of the composer (a tab of the main window, or a window of its own). Keys are bound in `assets/keymaps/default-macos.json`
//! (context `Composer`).

use std::ops::Range;

use gpui::actions;
use spelling::Lang;

actions!(
    composer,
    [
        /// Send the message (`cmd-enter`).
        Send,
        /// Close the window; asks whether to keep the draft if something changed
        /// (`escape`).
        Close,
        /// Save the draft now (`cmd-s`).
        SaveDraft,
        /// Attach files (`cmd-shift-a`).
        Attach,
        /// Show or hide the Cc and Bcc fields (`cmd-shift-c`).
        ToggleCcBcc,
        /// Move the draft from its tab to a window of its own.
        OpenInNewWindow,
    ]
);

/// Replace a misspelled word in the body with a suggestion (the body's right-click
/// menu). One undo step.
#[derive(Debug, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = composer, no_json)]
pub struct ReplaceMisspelling {
    /// The word's byte range in the body when the menu opened.
    pub range: Range<usize>,
    /// The misspelled word (nothing happens if the range no longer holds it).
    pub word: String,
    /// What to write instead.
    pub replacement: String,
}

/// Accept a word in this draft only ("Ignore Spelling" in the body's right-click menu).
#[derive(Debug, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = composer, no_json)]
pub struct IgnoreSpelling {
    /// The word, as written.
    pub word: String,
}

/// Add a word to the personal dictionary, for every draft ("Add to Dictionary").
#[derive(Debug, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = composer, no_json)]
pub struct AddToDictionary {
    /// The word, as written.
    pub word: String,
}

/// Check this draft in a language, or stop (the spelling status menu in the footer).
#[derive(Debug, Clone, PartialEq, Eq, gpui::Action)]
#[action(namespace = composer, no_json)]
pub struct ToggleSpellingLanguage {
    /// The language to turn on or off for this draft.
    pub lang: Lang,
}

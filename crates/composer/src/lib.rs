//! The Maze composer (layer L3, PLAN §4.10, CONCEPT §8.5).
//!
//! A message is written in a tab of the main window, titled with the draft's random
//! two-word name (`brave-otter`), next to the mail in a split if you like (ADR-018);
//! "Open in New Window" moves it to a window of its own. From (the identity picker), To, Cc and Bcc (collapsed until
//! needed), Subject and a Markdown body; `cmd-enter` sends, Escape or closing the
//! window asks whether to keep the draft when something changed. Drafts are autosaved
//! a moment after you stop typing.
//!
//! Like `mail_ui`, the composer doesn't depend on the core: everything goes through the
//! [`ComposeBackend`] trait, which the `mazemail` binary implements over
//! `mazemail_core` (and tests with a fake). It shares only `mazemail_types`' [`Draft`].
//!
//! To, Cc and Bcc hold the addresses as chips ([`recipients::RecipientField`]), with
//! completion from [`ComposeBackend::complete_address`] (address books and the
//! addresses you wrote to) and chips that can be dragged between the fields.
//!
//! The body's formatting keys (`cmd-b` bold, `cmd-i` italic, `cmd-k` link, …) write
//! Markdown around the selection (module `format`). They are bound in the `Composer`
//! context, so inside a composer they win over the main window's keys on the same
//! keystrokes (`cmd-k` Go to Folder, `cmd-b` the folder pane).
//!
//! Maze Magic (`cmd-m`) sends your text, without the quote below it, to an agent
//! through [`ComposeBackend::magic`] and shows the improved text over the composer:
//! Accept & Send (`enter`), Accept & Edit (`cmd-e`) or Discard (`escape`).
//!
//! The body is spell checked as you type (crate `spelling`, PLAN-spellcheck) in the
//! languages the app sets with [`set_spellcheck`] (the `spellcheck` setting); the
//! personal dictionary comes from and goes to the backend
//! ([`ComposeBackend::spelling_words`], [`ComposeBackend::add_spelling_word`]).

pub mod actions;
mod backend;
mod format;
pub mod recipients;
mod window;

use gpui::App;

pub use backend::{
    BoxFuture, ComposeBackend, ComposeError, IdentityChoice, MagicInput, SendOutcome,
};
pub use mazemail_types::compose::{Draft, DraftAttachment, DraftOrigin};
pub use spelling::Lang as SpellingLang;
pub use window::{
    COMPOSE_KIND, Composer, ComposerEvent, ComposerOptions, OpenComposerError, SPELLCHECK_DEBOUNCE,
    SerializedDraft, SpellcheckSettings, open_composer, set_spellcheck,
};

/// Call once at startup, after `ui::init`. The composer's keys (`cmd-enter` to send,
/// Escape to close, …) are in the default keymap's `Composer` context.
pub fn init(cx: &mut App) {
    let _ = cx;
}

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

pub mod actions;
mod backend;
mod window;

use gpui::App;

pub use backend::{BoxFuture, ComposeBackend, ComposeError, IdentityChoice, SendOutcome};
pub use mazemail_types::compose::{Draft, DraftAttachment, DraftOrigin};
pub use window::{
    COMPOSE_KIND, Composer, ComposerEvent, ComposerOptions, OpenComposerError, SerializedDraft,
    open_composer,
};

/// Call once at startup, after `ui::init`. The composer's keys (`cmd-enter` to send,
/// Escape to close, …) are in the default keymap's `Composer` context.
pub fn init(cx: &mut App) {
    let _ = cx;
}

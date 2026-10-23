//! The Maze compose window (layer L3, PLAN §4.10, CONCEPT §8.5).
//!
//! A message is written in its own window, the way Thunderbird does it: Maze's main
//! window has no tabs yet, and a separate window lets you look things up in the main
//! window while you write. From (the identity picker), To, Cc and Bcc (collapsed until
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
pub use window::{Composer, ComposerEvent, ComposerOptions, OpenComposerError, open_composer};

/// Call once at startup, after `ui::init`. The composer's keys (`cmd-enter` to send,
/// Escape to close, …) are in the default keymap's `Composer` context.
pub fn init(cx: &mut App) {
    let _ = cx;
}

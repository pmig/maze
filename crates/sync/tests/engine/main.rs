//! Integration tests of the sync engine against `imap::fake::FakeServer` and a real
//! (encrypted, temporary) store.

// Tests may unwrap (AGENTS.md); clippy only knows that for `#[test]` functions, not for
// the helpers they share.
#![allow(clippy::unwrap_used)]

mod bodies;
mod common;
mod folders;
mod gmail;
mod ops;
mod sync;

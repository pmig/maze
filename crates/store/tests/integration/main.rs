//! Integration tests for the store. One binary, several modules, to keep link times
//! down.

// Tests may unwrap (AGENTS.md); clippy only knows that for `#[test]` functions, not for
// the helpers they share.
#![allow(clippy::unwrap_used)]

mod aggregates;
mod blobs;
mod common;
mod contacts;
mod lists;
mod message_lists;
mod messages;
mod open;
mod ops;
mod perf;
mod plaintext;
mod remote_content;
mod secrets;
mod sorting;
mod spelling;

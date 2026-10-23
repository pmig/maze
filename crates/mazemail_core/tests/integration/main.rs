//! Integration tests for `mazemail_core` over a temporary data directory with an
//! in-memory key store. One binary, several modules, to keep link times down.

// Tests may unwrap and print (AGENTS.md); clippy only knows that for `#[test]`
// functions, not for the helpers they share.
#![allow(clippy::unwrap_used, clippy::print_stderr)]

mod accounts;
mod agent;
mod api;
mod bench;
mod commands;
mod common;
mod compose;
mod events;
mod index;
mod lifecycle;
mod messages;
mod queries;
mod search;

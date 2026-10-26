//! Integration tests for the importers, exporters and the body reuse index. One
//! binary, several modules, to keep link times down.

// Tests may unwrap (AGENTS.md); clippy only knows that for `#[test]` functions, not for
// the helpers they share.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

mod common;
mod perf;
mod reuse;
mod thunderbird;
mod upload;

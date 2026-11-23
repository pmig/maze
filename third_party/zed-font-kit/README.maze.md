# zed-font-kit 0.14.1-zed, patched for Maze

A copy of [`zed-font-kit` 0.14.1-zed](https://crates.io/crates/zed-font-kit/0.14.1-zed)
(MIT OR Apache-2.0, GPUI's font-kit fork), used through `[patch.crates-io]` in the
workspace `Cargo.toml`. Two changes, both in `Cargo.toml`:

- The Linux `freetype-sys` requirement is `0.23` instead of `0.20`.
- `[lints.rust] dead_code = "allow"`: some upstream code is unused on Linux
  (`Family::from_handle` and `from_font_handles`), and Cargo shows warnings for a path
  dependency that it hides for the same crate from crates.io.

Why: the original HTML engine was Servo (ADR-020), which linked `freetype-sys` 0.23 on
Linux, while GPUI 0.3.9 (through this crate) links 0.20; Cargo allows only one crate
that links the native `freetype` library in a dependency graph, on every target. The
bindings font-kit uses are the same in both versions. `examples/`, `tests/` and the
`Cargo.lock` of the published crate were left out.

Servo is gone (Blitz, ADR-033) and nothing else needs `freetype-sys` 0.23: remove this
copy and the patch entry, after a Linux CI run confirms the build without them.

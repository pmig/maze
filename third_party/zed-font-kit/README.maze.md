# zed-font-kit 0.14.1-zed, patched for Maze

A copy of [`zed-font-kit` 0.14.1-zed](https://crates.io/crates/zed-font-kit/0.14.1-zed)
(MIT OR Apache-2.0, GPUI's font-kit fork), used through `[patch.crates-io]` in the
workspace `Cargo.toml`. One change:

- `Cargo.toml`: the Linux `freetype-sys` requirement is `0.23` instead of `0.20`.

Why: the original HTML engine (Servo 0.7, ADR-020) links `freetype-sys` 0.23 on Linux,
GPUI 0.3.9 (through this crate) 0.20, and Cargo allows only one crate that links the
native `freetype` library in a dependency graph, on every target. The bindings font-kit
uses are the same in both versions. `examples/`, `tests/` and the `Cargo.lock` of the
published crate were left out.

Remove this copy (and the patch entry) once GPUI's font-kit or Servo agree on a
`freetype-sys` version.

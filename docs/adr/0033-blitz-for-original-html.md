# ADR-033: Blitz for original HTML

- **Status:** Accepted (supersedes the engine of [ADR-020](0020-servo-for-original-html.md);
  its modes, the document's origin and load policy and remote content stand)
- **Date:** 2026-11-23

## Context

ADR-020 put Servo behind the original HTML view. It worked and rendered mail
well, but it cost more than the view is worth:

- **A JavaScript engine Maze never runs.** Servo's DOM lives in SpiderMonkey's heap,
  so its interpreter can't be left out; only the CSP `sandbox`, `default-src 'none'`
  and switched-off JIT tiers kept scripts from running.
- **Build cost.** Without the JIT, `mozjs_sys` compiles SpiderMonkey from source:
  Python 3.11+, clang and libclang on every machine, minutes per clean build; the
  unsigned macOS build in CI ran into its 60-minute limit on every push to `main`.
- **Size.** The stripped release binary grew from 48.6 MB to 122.8 MB.
- **Dependency surgery.** Servo pinned `rusqlite` 0.38 and `freetype-sys` 0.23 for
  the whole workspace (a vendored `zed-font-kit`).
- **Churn.** `servo` 0.8 changed its embedding API and broke `mise start` and the
  release builds while CI, which builds without the engine, stayed green.

Two replacements were built and tested on branches with the same view API, the same
engine test and the same mail samples:

| | Blitz | litehtml |
| --- | --- | --- |
| Engine | Stylo (Firefox's CSS engine), Taffy layout, Parley text, vello_cpu raster; Rust | C++ (CSS 2.1), through the `litehtml` crate |
| JavaScript | none | none |
| Modern CSS (flex, grid, `border-radius` on images, gradients) | yes | partly (no flex or grid) |
| Legacy mail attributes (`<font>`, table `border`, `cellpadding`, `img align`) | not mapped by Blitz 0.3; Maze maps them (below) | mapped |
| Centered newsletter frame (`<td align=center>` around a table) | yes | no |
| Maze-side code | Rust only | a patched copy of the crate, an `unsafe` block, a C++ build |
| Cargo.lock | about 1140 crates | about 1090 crates |

A GPU rasterizer wasn't considered worth it: Blitz draws a Retina frame on the CPU in
a few milliseconds (below).

## Decision

The original view uses **Blitz** (`blitz-dom`, `blitz-html`, `blitz-paint`,
`blitz-traits` 0.3.0-beta.4, pinned exactly) with `anyrender_vello_cpu` for CPU
rasterizing. `html_view`'s feature is `blitz`, still exposed as `original-html` on
`mail_ui` and `mazemail`; `mise start` and the macOS release builds turn it on.

- **Only what mail needs.** No `blitz-net`, no `blitz-shell` (winit), no accessibility
  tree, no SVG, no web fonts, no ICU text transforms; `blitz-dom` with `system-fonts`
  and `floats` only, `anyrender_vello_cpu` with `multithreading` (up to 8 raster
  threads).
- **Rendering.** A view owns a page: one Blitz document per message (document
  number), laid out and painted on GPUI's main thread into an RGBA image that GPUI
  draws like any picture. The renderer is reset before every paint. In dev builds the
  engine's crates and `html_view` run at `opt-level = 3` (`[profile.dev.package]`):
  a Retina frame of a table newsletter takes 2.5 ms, a hover or scroll 1.7 ms.
- **Security** (docs/security.md). Blitz has no script engine, so nothing in a message
  can run. Every load goes through Maze's providers: the document and `cid:` parts
  from memory, remote images only through Maze's own client when the message's
  remote content decision allows it, everything else refused; Blitz gets no network
  provider of its own. Links are reported to Maze and follow the reader's link policy.
- **Mail fixes before the engine** (`mazemail_mime::html::hints`, in
  `rewrite_for_embedding`). Blitz 0.3 misses parts of the HTML table model and most
  presentational attributes, so Maze rewrites the sanitized HTML for it:
  presentational attributes (`<font>`, table `border`, `cellpadding`, `cellspacing`,
  `align`, `valign`, `img align`, `background`, `nowrap`) become rules of a generated
  style sheet with `:where()` selectors, as low in the cascade as a browser's hints;
  a `<td>` with a block `display` becomes a cell holding a `<div>`, as a browser's
  anonymous cell; empty spacer columns get no spare width next to columns with
  content; `colspan`s are fitted to the cells that exist. Each rule names the Blitz
  limitation it works around and can go when Blitz handles it.
- **Tests.** `html_view`'s engine tests (rendering, no network, no script, hover,
  cursor, copy, repaints), regression tests on two anonymized real mails (GitHub's
  Dependabot digest, Google Flights' price tracking) and 266 Web Platform Tests
  reftests from the table, presentational-hint and list directories, run through the
  sanitizer, the rewrite and Blitz (`crates/html_view/tests/wpt`). 102 pass;
  `expectations.txt` lists the others with the Blitz issue or Maze cause each waits
  for; the run fails when that list is out of date in either direction.

## Consequences

- **Builds.** No SpiderMonkey, no Python, no clang or libclang from mise; Cargo.lock
  drops from 1332 to 1142 crates. A first `mise start` with a warm compile cache
  takes under two minutes. The engine builds into `target/original-html` (was
  `target/servo`). The stripped macOS release binary is 84.5 MB, against 122.8 MB with
  Servo and 48.6 MB without an engine when ADR-020 was written.
- **Fidelity.** Modern CSS renders as in Firefox's style engine; the gaps are Blitz's
  table model (anonymous boxes from style sheets, collapsed borders, captions,
  percentage cell widths in automatic layout) and are tracked upstream (#1430, #386,
  #504, PR #529, #1435). A `@media` rule that turns cells into blocks on narrow
  panes is not covered by the rewrite.
- **A beta dependency.** Blitz 0.3 is a beta and its API changes between releases;
  it's pinned exactly. An upgrade shows up in the WPT run as tests that start
  passing (several presentational attributes are already mapped on Blitz's `main`).
  The Maze-side rewrite rules should shrink with each upgrade.
- **Leftovers of ADR-020 that can go.** The `rusqlite` 0.38 pin (only Servo forced it;
  the SQLite advisory needs a SQLCipher 4.20 bundle either way) and the vendored
  `zed-font-kit` (needs a Linux CI run without it).
- **Linux.** Nothing in Blitz needs Python or libclang, so Linux release builds could
  turn the original view on; that still needs testing on Linux.
- **GPUI tests with the engine.** `mail_ui`'s tests pass with `original-html` on;
  ADR-020 had to keep them engine-free. CI doesn't build the engine yet, so its
  tests run locally (`cargo nextest run -p html_view --features blitz`).
- **The sanitizer drops attributes of `<body>`** (`style`, `bgcolor`, `background`).
  Mail sets its canvas color and default font there; the WPT run shows it. It
  affects every view, not only Blitz.

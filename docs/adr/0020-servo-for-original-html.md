# ADR-020: Servo for original HTML

- **Status:** Accepted (supersedes [ADR-004](0004-html-original-mode.md))
- **Date:** 2026-10-26

## Context

Reader mode (gpui-kit `TextView` over the sanitized HTML, PLAN §4.8) reads most mail
well but can't lay out what designers send: newsletters, receipts and Outlook tables
need real CSS layout. ADR-004 kept an "original mode" out of the MVP and named Servo,
then litehtml, as candidates. Users asked for full HTML: by default in a message tab
(double-click, `enter`, `cmd-o`), with a Reader/Original switch in both the message
pane and tabs.

The pipeline is engine-independent: `mazemail_mime` sanitizes (ammonia allowlist;
scripts, frames, objects, forms, `meta refresh`, `base`, `on*` attributes and
`javascript:` URLs removed; tracking pixels dropped) and rewrites `cid:` and remote URLs
to one resource origin. An engine only adds an output after "rewrite resources".

Options compared:

| | gpui-kit `TextView` (reader) | litehtml | System WebView (WKWebView / WebKitGTK via Wry, `gpui-webview`) | Servo (`servo` 0.7 from crates.io) |
| --- | --- | --- | --- | --- |
| Fidelity | Simplified, no CSS layout | CSS 2.1 tables, no modern CSS | Same engine as Apple Mail | Modern CSS (flex, grid, tables), WebRender |
| Platforms | All | All (C++ dependency) | Different engine per OS; Windows WebView2 covers overlays | One engine everywhere, in Rust |
| Embedding in GPUI | Native element | Would need a painter for GPUI | Native NSView over the window: layering, focus, clipping (needs GPUI Fast for composition; we are on gpui-pre) | Offscreen software rendering into an image: a normal GPUI element |
| JavaScript | None | None | Off by a preference | Interpreter always compiled in; off through CSP `sandbox` + `default-src 'none'` (verified) |
| Load interception | n/a | Embedder callbacks | Custom scheme + navigation handler | `WebViewDelegate::load_web_resource` for every load |
| Build / size | 0 | Small | Near 0 (system framework) | Minutes of build, a large binary share (see below) |
| System dependency | None | None | WebKit of the OS (version varies) | None beyond fonts |

The user chose Servo: one engine on every platform, written in Rust, no dependency on
the system's WebKit version, and a renderer we can run offscreen so it behaves like any
other GPUI element. The system WebView was the earlier lead pick for its zero size and
Apple-Mail fidelity; it loses on embedding (a native view that covers overlays and takes
keyboard focus with gpui-pre) and on having a different engine per platform.

## Decision

Original mode uses **Servo** in a new L3 crate, `html_view`, behind the cargo feature
`servo` (exposed as `original-html` on `mail_ui` and `mazemail`). `servo` 0.7 is built
from crates.io with `default-features = false` plus `baked-in-resources`: no `js_jit`
(no JIT entitlement), no `multiprocess`, no devtools, no clipboard backend, none of
WebGL/WebGPU/WebXR/WebCrypto/Bluetooth/GStreamer.

- **Rendering:** one Servo instance per process, started on first use, on GPUI's main
  thread; its event-loop waker schedules a spin from a foreground task. Each view owns a
  Servo `WebView` with its own `SoftwareRenderingContext`; frames are read out and
  painted as a GPUI image. Mouse, wheel and copy go to Servo. No native view exists, so
  clipping, overlays (palette, Quick Move, menus, dialogs, notifications), hiding with
  the tab and keyboard focus are GPUI's as for any element.
- **One page per view, reused:** a view shows the next message in the same Servo page.
  Each document gets a number and is served at
  `https://message.mazemail.localhost/m<n>/message.html` with its resources under
  `/m<n>/`; loads of an older number are refused, a new document loads after 40 ms (so
  holding `f` doesn't queue renders), and the reader stays visible until the new
  document has painted (also when the engine can't start or the feature is off).
- **Security** (docs/security.md): the document is the sanitized HTML with
  `Content-Security-Policy: sandbox; default-src 'none'; img-src <origin> data:;
  style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'` as a response header
  (and the CSP, without `sandbox`, as a `<meta>`). Servo applies CSP-derived sandbox
  flags, so scripting is disabled for the page; a headless test loads a page with a
  script and `onload` into the real engine and checks that neither ran with the full
  header, with `sandbox` alone and with the CSP alone. Every load goes through
  `load_web_resource`: the document and `cid:` parts are answered from memory (parts
  are read on a background thread), remote images only when the message's remote
  content decision allows it and fetched by Maze's own HTTP client, everything else
  gets an empty 404. Navigation is denied except to the current document; `http(s)` and
  `mailto` clicks are handed to the reader's link policy (`mail_ui::links::open_link`:
  deceptive links ask, `mailto:` starts a draft), and Servo's status text drives the
  same hover preview as the reader.
  Preferences turn off the JIT tiers, WebAssembly, devtools, workers, IndexedDB and web
  APIs; `Opts::temporary_storage` and no config dir keep nothing on disk; `hard_fail`
  is off so an engine crash doesn't end Maze.
- **Modes:** `reader.default_mode` (pane, default `reader`) and `reader.tab_mode` (tabs,
  default `original`); the Reader/Original buttons and `cmd-alt-o` switch the shown
  message. Plain-text mail never uses the engine.
- **Remote content:** `reader.remote_content` (per account) is enforced by the core:
  `never` never fetches, `always` fetches, `ask` blocks and offers "Load images" (this
  message, not kept) and "Always for this sender" (store table `remote_content_sender`,
  schema v11). `never` wins over the sender list. Inline parts always show.
- **Builds:** macOS release builds (`script/bundle-mac`) and `mise start` turn the
  feature on; a plain `cargo build`, CI clippy and the test suite don't, and then the
  view says the original is in release builds and shows the reader. The Servo engine
  has its own headless tests (`cargo test -p html_view --features servo`).

## Consequences

- **Dependency surgery.** Cargo allows one crate per native `links` name, and Servo
  0.7 disagrees with us twice: `servo-net` links SQLite through `rusqlite` 0.38, so
  the workspace moves from `rusqlite` 0.40 to 0.38 (`rusqlite_migration` 2.4; SQLCipher
  stays, through `bundled-sqlcipher`); and Servo links `freetype-sys` 0.23 on Linux
  while GPUI's font-kit fork wants 0.20, so `third_party/zed-font-kit` (one changed line,
  README.maze.md) is patched in. Both go away when the versions line up again.
- **Build requirements.** Servo's build scripts need Python 3.11+ (or `uv`), and
  `mozjs_sys` downloads a prebuilt SpiderMonkey static library from Servo's GitHub
  releases on the first build (it builds from source with `MOZJS_FROM_SOURCE`, which
  needs clang). Clean build of the Servo crates: about 3 m 45 s debug; the release
  build with Servo took 10.5 min against 7.6 min without (fat LTO).
- **Size.** The stripped macOS release binary (fat LTO) grows from 48.6 MB (the same as
  `main` today, already above the 40 MB budget of CONCEPT §3) to 122.8 MB, +74 MB.
  Whether to raise the budget is the user's call.
- **Memory and speed** (debug build, M-series Mac, a 200-story newsletter at 800×900
  points on a 2× display): starting Servo and the first view about +125 MB resident,
  each further view about +22 MB; first frame 30–60 ms after the document is handed
  over, a following message in the same view about 55–125 ms plus the 40 ms load delay.
- **Rebuilds.** Turning the feature on costs about 3 minutes once; afterwards a change
  in `mail_ui` rebuilds in 5.2 s instead of 3.7 s, so `mise start` includes it.
- **Advisories.** Six "unmaintained" advisories come in through Servo (bincode 1 via
  WebRender, `unic-*` via urlpattern) and are ignored in `deny.toml` with reasons, like
  GPUI's.
- **Linux.** The engine is not enabled in Linux releases yet (WebRender's software
  path and the Python build requirement are unverified on our Linux CI); Linux shows the
  reader. The font-kit patch affects every Linux build and needs a CI run there.
- **Not yet:** keyboard scrolling and `find` inside the original, `cmd-a` select all,
  "force dark" for designed mail, printing, "Open in browser" (PLAN §4.8, still to be
  built), and fetching remote images in parallel batches.
- **GPUI tests** run without the engine; with `--features original-html` the test
  scheduler flags Servo's background threads as non-determinism, so the GPUI tests
  stay engine-free and the engine is tested headlessly in `html_view`.

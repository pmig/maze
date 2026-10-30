# Third-party code and data

Code and data in this repository that Maze didn't write, with their licenses. Crates
pulled from crates.io are not listed here; `cargo deny check` (`deny.toml`) keeps their
licenses compatible with Maze's GPL-3.0-or-later.

| Directory | What | License | Used by |
|---|---|---|---|
| [`zed-font-kit/`](zed-font-kit/README.maze.md) | GPUI's font-kit fork 0.14.1-zed, one patched requirement | MIT OR Apache-2.0 | GPUI, via `[patch.crates-io]` |
| [`dictionaries/`](dictionaries/README.md) | Hunspell dictionaries en-US, en-GB (SCOWL) | MIT AND BSD | `spelling` (embedded in the binary) |
| [`dictionaries/`](dictionaries/README.md) | Hunspell dictionaries de-DE, de-AT, de-CH (igerman98) | GPL-2.0 OR GPL-3.0, distributed under GPL-3.0 | `spelling` (embedded in the binary) |

Everything here that ends up in the Maze binary must be named, with its license, on the
licenses screen once there is one.

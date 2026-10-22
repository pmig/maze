# Maze

An open source, local-first email client written in Rust. Built like
[Zed](https://zed.dev) (GPU-rendered, fast, small, agent-native) and behaving like
[Thunderbird](https://www.thunderbird.net) (same layout, same shortcuts).

> **Status:** pre-alpha. Nothing here is ready for daily use yet.

The app is called **Maze**. Every technical identifier (crate, binary, CLI, data
directories, environment variables) is **`mazemail`**, because the `maze` crate name is
taken.

## Building

The toolchain is managed with [mise](https://mise.jdx.dev):

```sh
mise install          # Rust 1.99.1, cargo-nextest, cargo-deny, cargo-insta
mise run ci           # fmt check, clippy, tests, cargo-deny
cargo run             # starts Maze
cargo run -- --help   # the mazemail CLI
```

macOS (Apple Silicon) is the primary platform. Linux comes next.

## Repository layout

Crates live in `crates/`. The layering rules and conventions are in
[`AGENTS.md`](AGENTS.md), architecture decisions in [`docs/adr/`](docs/adr/).

## License

GPL-3.0-or-later, see [`LICENSE`](LICENSE).

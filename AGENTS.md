# Working on Maze (mazemail)

Instructions for humans and coding agents working in this repository.

## Names

- The product is **Maze**. Use "Maze" wherever people read it: window titles, menus,
  dialogs, notifications, docs, `Maze.app`.
- Every technical identifier is **`mazemail`**: crate and binary names, the CLI
  (`mazemail search …`), env vars (`MAZEMAIL_*`), token prefix `mazemail_s_`,
  data/config directories, the socket `mazemail.sock`, bundle ID `dev.mazemail.maze`.

## Layering (enforced by dependencies)

```
L4  mazemail (binary: GUI + CLI)
L3  mail_ui · composer · agent_ui · auto_update_ui · html_view · ui        (GPUI)
L2  mazemail_core · local_api · agent · assistant · import · rules
L1  store · search · imap · smtp · auth · security · sync · auto_update · vault ·
    sandbox · settings
L0  mazemail_types · mazemail_mime · release_channel           (pure, no IO)
```

Crate names follow PLAN §2, except where a bare name would collide: `core` (Rust's
`core` library) is `mazemail_core`, and `mime` (a popular crates.io crate) is
`mazemail_mime`. The importer is `import` (mbox, Maildir and Thunderbird).

- A crate may only depend on crates in lower layers (same-layer deps only where noted
  in the crate's docs).
- Nothing below L3 depends on GPUI. `mazemail_core` runs on Tokio, so the CLI, headless mode
  and tests use it without a window.
- The UI never touches `store` directly; it goes through `mazemail_core`.

## Conventions

- Rust 2024 edition, toolchain pinned in `rust-toolchain.toml`. Run `mise run ci`
  before committing (fmt, clippy `-D warnings`, nextest, cargo-deny).
- Add external dependencies to `[workspace.dependencies]` in the root `Cargo.toml`
  and use `dep = { workspace = true }` in crates. Check licenses against `deny.toml`.
- Every crate has `[lints] workspace = true`. `unsafe` is denied by default; a crate
  that truly needs it allows it locally with a `// SAFETY:` comment on every block.
- Errors: `thiserror` in library crates, `anyhow` only in binaries, xtask and tests.
- Logging with `tracing`. Message content, subjects and addresses are never logged in
  plain text: wrap them in `mazemail_types::Sensitive<T>`.
- No `unwrap()`/`expect()` in library code paths that can fail at runtime. Tests may
  unwrap.
- Tests next to the code (`#[cfg(test)]`) for units, `tests/` for integration tests.
  Snapshot tests use `insta`.
- Mail data on disk is always encrypted (`vault`). Never write message content in
  plaintext outside the documented exceptions (PLAN §4.17).

## Git

- Branches are worktrees: `git -C <repo> worktree add <name> -b <name>`.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org)
  (`feat(store): …`, `fix(imap): …`, `docs: …`, `ci: …`, `chore: …`). Each branch lands
  on `main` as a single squashed conventional commit, which release-please reads.

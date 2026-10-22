# ADR-001: License

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Maze wants to reuse code from Zed's GPL crates (`workspace`, `settings`, `agent_servers`) and to adapt Zed's `auto_update` and `sandbox` crates, which are GPL-3.0-or-later. CONCEPT.md §19 lists Apache-2.0 as the alternative, which would only work if we never copy Zed app crates.

## Decision

Maze is licensed under **GPL-3.0-or-later**.

## Consequences

- We can copy and adapt Zed's GPL crates, including the updater (PLAN §4.15) and the agent sandbox (PLAN §4.18, ADR-013).
- All dependencies must be compatible with GPL-3.0-or-later. The ones planned so far are Apache, MIT and MPL. `deny.toml` holds the license allowlist and `cargo deny check` runs in CI.
- The root `LICENSE` file and `license = "GPL-3.0-or-later"` in `[workspace.package]` state it.

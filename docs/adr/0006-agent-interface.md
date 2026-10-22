# ADR-006: Agent interface

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Agents need a way to read and act on mail without direct access to the store. This was a user decision. mxr uses the same daemon-plus-socket pattern successfully.

## Decision

Agents use the **`mazemail` CLI over a local Unix socket with scoped tokens** (PLAN §4.11). Agents Maze runs are driven over **ACP v1** (`agent-client-protocol` 3.x builder API; the reference clone is 3.3.0, crates.io has 3.5.0 on 2026-10-22, pinned in S4), inside an OS sandbox (ADR-013).

## Consequences

- Every request that touches mail needs a valid token. There is no tokenless profile. Tokens are created by sessions Maze starts, or by `mazemail login` after an approval dialog.
- The CLI never opens the database, so it never needs the storage key.
- GUI, CLI and `mazemail --headless` are one binary. The CLI inside the bundle is updated together with the app (PLAN §4.15).
- Spike S4 tests ACP with Claude Code and Codex and the sandbox together.

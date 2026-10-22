# ADR-013: Agent sandbox

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Tokens only protect the CLI path. Agents have their own file tools, so without a sandbox they could read the store directly. Zed's `sandbox` crate is GPL-3.0-or-later, so ADR-001 lets us adapt it.

## Decision

Every agent process Maze starts, and every terminal command it runs for an agent, runs in an **OS sandbox** that can't read `<data_dir>/mail/` or the Thunderbird profile. The sandbox uses **Seatbelt on macOS** and **bubblewrap plus seccomp on Linux**, adapted from Zed's `sandbox` crate (PLAN §4.18).

## Consequences

- Maze hides the whole home directory from agents, allows reads only for the agent's own home, the session scratch dir, installed agents, runtimes and the bundled CLI, and allows only `mazemail.sock` among Unix sockets. Zed's policy only controls writes, so we add read denial.
- Agents you start yourself outside Maze aren't sandboxed by Maze. For them, encryption at rest (ADR-012) and approved tokens (ADR-006) apply.
- Risk R15: agents may not work inside the sandbox (nested Seatbelt, hidden home paths, Linux without `bwrap`). Spike S4 checks Claude Code and Codex. There is a settings opt-out with a visible warning.
- `sandbox-exec` is deprecated but used by Zed, Codex and Chromium.

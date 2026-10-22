# ADR-016: Cursor and pi as ACP agents

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Maze should work with Cursor and with the pi coding agent. Zed supports both as
ACP-integrated external agents (`zed-industries--zed/docs/src/ai/external-agents.md`):
Cursor through Cursor's own agent CLI, pi through the `pi-acp` adapter. Zed states that a
Cursor subscription does not configure its own LLM providers. Both agents are listed in
the ACP registry (`agentclientprotocol--registry/cursor`, `…/pi-acp`).

## Decision

Cursor and pi are supported as **ACP agents from the registry**, exactly like other
external agents (PLAN §4.12):

- **Cursor:** the registry's `binary` distribution, `cursor-agent acp`, downloaded from
  Cursor on demand (proprietary, never redistributed by Maze). The user signs in with
  their Cursor account through the agent's ACP auth method.
- **pi:** the `pi-acp` `npx` package plus `@earendil-works/pi-coding-agent`, installed
  into the agent's directory on the Maze-managed Node runtime (Node 22+). `pi-acp`
  starts `pi --mode rpc`.

A Cursor subscription does not become a model provider for Maze's built-in assistant,
and Maze does not use Cursor's private APIs.

## Consequences

- Both agents reach mail only through the `mazemail` CLI with a scoped session token,
  and both run inside the agent sandbox (ADR-013) with their own agent home, so their
  logins and settings (`~/.cursor`, `~/.pi`) are separate from the user's terminal
  setups.
- pi runs its own shell and file tools without permission prompts and not through ACP's
  `terminal`, so for pi the sandbox is the only boundary; the agent column states this.
- Cursor's agent may sandbox itself; spike S4 checks it like Claude Code and Codex and
  sets the per-agent "already sandboxed" option if needed.
- The managed Node runtime must be Node 22 or newer.

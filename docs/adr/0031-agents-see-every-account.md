# ADR-031: Agents see every account

- **Status:** Accepted
- **Date:** 2026-11-18

## Context

Until now AI was off per account: `accounts.<name>.agent` (earlier `agent.accounts`)
was `off` by default, `local_only` or `any`, and only `any` accounts were ever in a
token's scope, in an agent session or in Maze Magic. `local_only` was never usable,
because Maze can't tell which agents run on local models. In practice it was one more
switch to find before an agent could do anything: someone who sets up an agent wants
it to read their mail.

## Decision

The per-account setting is gone. No mail reaches an agent until the user starts one
(a conversation in the agent column, or Maze Magic); an agent that is started can read
every account, within its session's profile and its sandbox.

The local API keeps resolving a token's scope on every request, but against the
accounts that exist (`AccountList`) instead of a policy: a token for every account
covers accounts added later, and removed accounts drop out of every scope. A token the
user creates can still be limited to accounts (`mazemail login --account …`).

## Consequences

- Fewer refusals and no settings to edit before the first conversation. The agent
  column's "no account shares mail" banner and Maze Magic's account check are gone.
- The privacy promise is now "only when you start an agent" instead of "off per
  account". The docs, the website and security.md say so.
- Someone who wants an account out of reach of agents has to keep agents unconfigured,
  or give external agents tokens limited to other accounts. A per-account opt-out can
  come back if people ask for it.

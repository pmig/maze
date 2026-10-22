# ADR-007: Gmail sync

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Gmail exposes every label as an IMAP folder, so syncing per folder stores one copy of a message per label. Thunderbird downloads per folder, which is a big part of its 24 GB.

## Decision

For Gmail, sync `[Gmail]/All Mail`, Trash and Spam only. Labels delivered via `X-GM-LABELS` become folder memberships.

## Consequences

- One copy per message instead of one per label (PLAN §4.5).
- Gmail needs the X-GM-* extensions in the IMAP layer (ADR-003). Spike S2 checks the label mapping.
- Replay tests cover that one message with several labels is stored once (by `gm_msgid`) with its own flags per location, and that delete means move to Trash (PLAN §7).

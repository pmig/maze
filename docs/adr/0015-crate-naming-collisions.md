# ADR-015: Crate naming collisions

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

PLAN §2 names the crates `core` and `mime`. A crate called `core` collides with Rust's `core` library, and `mime` is a popular crates.io crate. PLAN §2 also lists the importer as `import_thunderbird`, while the importer is meant to cover more than Thunderbird.

## Decision

The crates are named `mazemail_core` (instead of `core`) and `mazemail_mime` (instead of `mime`). The importer crate is `import` and handles mbox, Maildir and Thunderbird. All other crate names follow PLAN §2. Layering is unchanged (AGENTS.md).

## Consequences

- `mazemail_core` is the L2 facade (PLAN §4.2) and `mazemail_mime` is an L0 crate, next to `mazemail_types` and `release_channel`.
- Names in PLAN.md that still say `core`, `mime` or `import_thunderbird` mean these crates.
- Dependency declarations in `[workspace.dependencies]` use the new names.

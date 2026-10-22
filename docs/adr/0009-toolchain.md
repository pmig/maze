# ADR-009: Toolchain

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

We want one reproducible toolchain that every developer and CI use. Zed pins 1.98.1, and Rust 1.99.1 is the latest stable. CONCEPT §18 asks for the toolchain through `mise` and no Homebrew.

## Decision

Rust **1.99.1** is pinned in `rust-toolchain.toml` (latest stable), edition 2024, and installed via **`mise`**. `mise.toml` also provides cargo-nextest, cargo-deny and cargo-insta.

## Consequences

- One toolchain, no Homebrew. `mise run ci` runs fmt, clippy with `-D warnings`, nextest and cargo-deny, and CI runs the same tasks (PLAN §3).
- Spike S1 confirms that the GPUI stack builds with 1.99.1.

# ADR-004: HTML original mode

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

An embedded HTML engine means, for Servo, a helper process, a JIT entitlement, about 100 MB and a slow build, for mail that reader mode mostly handles. The sanitizer and resource rewrite (PLAN §4.8) do not depend on the engine.

## Decision

**No HTML original mode in the MVP.** Reader mode is the only in-app renderer, plus "Open in browser" (PLAN §4.8). An embedded engine (Servo first, then litehtml) is evaluated later.

## Consequences

- M0 and M3 stay small.
- Adding an engine later does not change the pipeline, because the sanitizer and resource rewrite are engine-independent.
- Reader mode quality on complex HTML (newsletters, receipts, Outlook tables) is risk R2. Spike S3 measures it on 50 real emails (target: at least 40 readable without the browser).
- An embedded renderer stays on the Later list.

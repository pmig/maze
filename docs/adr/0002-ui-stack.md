# ADR-002: UI stack

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

The UI has to deliver a multi-column mail layout, a very long virtualized message list, a multi-line editor and an HTML-capable text view on GPUI. Building all of that from scratch is a large cost. gpui-kit provides dock and resizable panels, `VirtualList`, a sortable and resizable `Table`, `Tree`, a multi-line `Editor`, `TextView::html/markdown`, a command palette, notifications and `gpui-webview` (`longbridge--gpui-kit/crates/component/src/*`). It is Apache-2.0 licensed. The reference clone is 0.7.3. Version 0.8.0 (2026-10-19) keeps the same `gpui-pre` pin.

## Decision

Use **gpui-kit 0.8.x** on its pinned GPUI snapshot (`gpui-pre =0.3.9`). Spike S1 starts on 0.8. We follow gpui-kit's GPUI pin instead of Zed's published `gpui` crate (CONCEPT §19, item 5).

## Consequences

- Spike S1 (PLAN §6) has to confirm the 4-column layout, a 1M-row list at 120 fps, a 10k-row Table, the Zed theme mapping, and that `gpui_tokio` works with `gpui-pre`.
- We are tied to gpui-kit's GPUI snapshot and its upgrade pace.
- ADR-008 (compose editor) and ADR-004 (reader mode) depend on gpui-kit's `Editor` and `TextView`.

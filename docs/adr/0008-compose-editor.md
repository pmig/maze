# ADR-008: Compose editor

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

A true rich-text editor on GPUI is a large unknown (risk R4).

## Decision

The composer is a **Markdown-first editor built on the gpui-kit `Editor`**. It generates `multipart/alternative`: text/plain as the Markdown source, and text/html rendered from the Markdown with mail CSS (PLAN §4.10).

## Consequences

- Rich-text editing is deferred.
- Replying to HTML mail converts the quote from HTML to Markdown, which is lossy. An option keeps the original HTML quote in the HTML part.
- Spike S6 checks typing latency, IME and German umlauts, spell-check underlines, and that the output renders correctly in Gmail and Outlook web.

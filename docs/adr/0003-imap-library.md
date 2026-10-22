# ADR-003: IMAP library

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

`imap-codec 2.0-alpha` lacks Gmail X-GM-*, SPECIAL-USE and ESEARCH, and its CONDSTORE/QRESYNC support is "unfinished" (`duesee--imap-codec/imap-types/src/lib.rs`). mxr ships QRESYNC, IDLE and X-GM-THRID on its fork `mxr-async-imap` 0.10.6 (ENABLE, NAMESPACE, `select_qresync`, raw `run_command`). That fork still drops `APPENDUID`/`COPYUID` (`planetaryescape--mxr/crates/provider-imap/src/session.rs`, `uid_append`) and runs on async-std with native TLS.

## Decision

Maintain **our own fork of `async-imap`** (imap-proto) behind our own `ImapSession` trait (PLAN §4.5), on the `runtime-tokio` feature with `tokio-rustls`. We fork once, add the missing response codes (`APPENDUID`/`COPYUID`), and stay on Tokio and rustls.

## Consequences

- The rest of Maze depends on the `ImapSession` trait only, so the library can be replaced later. `imap-next` is a candidate once its CONDSTORE/QRESYNC support is finished and Gmail extensions exist (CONCEPT §19, item 6).
- We own the fork and its maintenance.
- Spike S2 (PLAN §6) checks QRESYNC, IDLE, X-GM-LABELS/THRID, MOVE, COMPRESS, APPENDUID/COPYUID and NOTIFY against Gmail, Microsoft 365, Stalwart and the uberspace host.

# ADR-005: Storage

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Maze has to hold very large mailboxes locally (the reference setup is a 24 GB Thunderbird profile), list and search them fast, and keep everything encrypted at rest (ADR-012). Thunderbird's Panorama moves the same way, to a single database (`mailnews/db/panorama/src/DatabaseCore.cpp`).

## Decision

Metadata lives in **SQLite (WAL)**, message content in **content-addressed zstd blobs**, and search in **Tantivy**. All of it is encrypted at rest (ADR-012).

## Consequences

- The store design is in PLAN §4.3 (schema, writer and reader connections, migrations), search in §4.4, and the encrypted formats in §4.17.
- SQLCipher and an encrypted Tantivy `Directory` rule out memory-mapped I/O. This is risk R13, and spike S7 measures the cost against the budgets, with an FTS5 fallback for search.
- Blobs are immutable, and Tantivy can be rebuilt, so only the database needs migration backups (PLAN §4.15).

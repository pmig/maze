# ADR-012: Encryption at rest

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Agents running outside Maze, other programs running as you, backups and synced copies of the data directory must not be able to read mail.

## Decision

Everything under `<data_dir>/mail/` is encrypted: **SQLCipher** for `mazemail.db`, **XChaCha20-Poly1305** for blobs and caches, and an encrypted Tantivy `Directory`. One random master key lives in the OS keychain, and subkeys are derived via HKDF (PLAN §4.17). SQLCipher is available in `rusqlite` as the `bundled-sqlcipher` feature.

## Consequences

- It protects against programs that don't go through Maze and against copies of the data directory. It does not protect against root, a process that can debug Maze, or plaintext Maze hands out on purpose. On Linux it is weaker, because any process running as you can read an unlocked Secret Service collection.
- On macOS the key uses the data protection keychain with the access group `<TeamID>.dev.mazemail.maze`, which needs an embedded Developer ID provisioning profile (`script/bundle-mac` embeds it). The fallback is the login keychain with an ACL.
- No mmap for SQLCipher or Tantivy (risk R13). Spike S7 measures the cost, with an FTS5 fallback for search.
- A lost key means a re-sync. Settings offer a recovery key (PLAN §4.17).
- Encryption is on from the first commit of the data layer, so there is never a plaintext store to migrate.

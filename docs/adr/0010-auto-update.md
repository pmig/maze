# ADR-010: Auto-update from day one

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Releasing early and often only works if updating costs nothing. Zed's flow is proven (`zed-industries--zed/crates/auto_update`), but its server is closed source.

## Decision

A **Zed-style updater** (PLAN §4.15) ships in M0, so the first alpha release can already update itself to the next one. Updates come from a **static signed manifest plus GitHub Releases**, with no update server. Manifests are Ed25519-signed and contain the SHA-256 of each artifact, and clients send nothing but a plain GET.

**No Homebrew** (cask, tap or formula) and no other macOS package manager. The `.dmg` is the only first-install path, so every install updates itself.

## Consequences

- On macOS the only distribution is the `.dmg` on GitHub Releases plus the signed update manifests. A package manager would compete with the updater for the same bundle. No release workflow or script publishes to Homebrew.
- On Linux the tarball stays. Flatpak or distro packages remain a later option, with the updater disabled by a build flag (`MAZEMAIL_UPDATE_EXPLANATION`).
- The updater, bundle swap, rollback and pre-migration backups are specified in PLAN §4.15. Release artifacts and manifests are produced only for releases (ADR-011).
- The public signing key is compiled into the binary and the private key lives only in CI secrets (`UPDATE_SIGNING_KEY`). Two keys are supported at once so the key can be rotated.

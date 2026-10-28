# ADR-011: Release process

- **Status:** Accepted (working decision, may be revised by M0 spikes)
- **Date:** 2026-10-22

## Context

Releases should be deliberate, versioned and come with a changelog. Maze uses the same tooling and conventions as `distr-sh/distr` and `godatei/datei`: conventional PR titles, `release-please-action` v5 and tags without `v`.

## Decision

Releases use **release-please** (PLAN §4.16). Merges to `main` only run CI. A release happens when the release-please PR is merged. **There are no nightly builds.**

PR titles are conventional commits (enforced by `semantic-pr.yaml`) and PRs are squash-merged. `release-type: simple` maintains `version.txt` and `CHANGELOG.md`, tags have no `v` prefix (`0.2.0-alpha.3`), and prereleases go to the `preview` channel and final versions to `stable`.

## Consequences

- release-please's `rust` updater rejects a Cargo workspace root, so `version.txt` is the single source of truth. All crates keep `version = "0.0.0"` and `publish = false`, so a release never changes `Cargo.lock` and `cargo build --locked` keeps working.
- release-please runs with the built-in `GITHUB_TOKEN`, without a bot token (as in `distr-sh/distr-infrastructure-controllers`). Tags created with it don't trigger `release-artifacts.yaml`, so `release-please.yaml` starts it on the new tag with `gh workflow run`, and the release PR it opens doesn't start CI by itself (close and reopen it to run the required checks). `release-artifacts.yaml` publishes to `gh-pages` with `GITHUB_TOKEN` as well. (Until October 2026 a `RELEASE_BOT_TOKEN` secret was used for both.)
- Maintenance branches `v*.*.x` and `v*.*.*-rc.x` get their own release PRs, as in distr.
- Builds between releases are only workflow artifacts with 7-day retention, never published and never offered by the updater.

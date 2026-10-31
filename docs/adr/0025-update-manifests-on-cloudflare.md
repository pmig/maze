# ADR-025: Update manifests on a Cloudflare Worker

- **Status:** Accepted
- **Date:** 2026-10-30

## Context

The updater fetches `https://releases.mazemail.dev/<channel>/<os>-<arch>.json`
(ADR-010, PLAN §4.15). Until now `release-artifacts.yaml` committed those signed
manifests to a `gh-pages` branch and GitHub Pages served them, with a CNAME
`releases.mazemail.dev → pmig.github.io`. That meant a second branch with a git history
of manifests, a push retry loop, `CNAME`/`.nojekyll` files, a Pages setting and a
certificate GitHub issues on its own schedule, and no control over response headers or
caching (Pages sends `max-age=600`). The website already runs on Cloudflare Workers in
the same zone (`website/`), with the deploy tooling (pinned wrangler, wrangler-action)
in place.

## Decision

The manifests are served by a small Worker, **`releases`** (`releases/`), on
the custom domain `releases.mazemail.dev`. It reads them from a **Workers KV** namespace
bound as `MANIFESTS`, whose keys are the URL paths (`stable/macos-aarch64.json`). It
answers `GET`/`HEAD` only for the strict pattern
`/<stable|preview>/<macos|linux>-<aarch64|x86_64>.json`, with
`Cache-Control: public, max-age=60`; everything else is a 404 and other methods a 405.
Observability is off and it has no `workers.dev` or preview URL, so nothing about the
clients is recorded, as on the website.

`publish-manifests` in `release-artifacts.yaml` reads the published manifests from KV,
runs `script/generate-manifests` unchanged (signing, channels, the no-downgrade and
preview-not-behind-stable rules) and writes the changed manifests with one
`wrangler kv bulk put`, still only after every build has uploaded its assets.

The `.dmg` and `.tar.zst` downloads stay on GitHub Releases.

## Consequences

- No app change: the URL and the signed format are the same, and the host still doesn't
  have to be trusted, because the app verifies the Ed25519 signature and the SHA-256.
  This is still a static manifest in the sense of ADR-010: the Worker returns a stored
  value and does nothing per client.
- Releases and rollbacks reach clients in about two minutes (KV propagation plus 60
  seconds of caching) instead of a Pages build plus ten minutes of caching. A rollback
  is a `wrangler kv key put` of the older manifest (docs/releasing.md), not a revert
  on `gh-pages`.
- KV is the only copy of the published manifests; there is no history like the
  `gh-pages` log. They can always be regenerated from a release's assets and the
  signing key.
- `release-artifacts.yaml` needs a Cloudflare API token that can only write KV, in a
  GitHub environment `manifests` that allows release tags only, instead of
  `contents: write` on the repository; the publish job is now read-only on GitHub. The
  Worker is deployed from `main` with a second token (Workers Scripts and Routes, no
  KV) in the environment `releases`.
- `releases.mazemail.dev` now depends on Cloudflare like `mazemail.dev` does. GitHub
  Pages is off; `mazemail.dev` stays a verified Pages domain so no one else can claim
  the subdomain there.
- Changes in `releases/` deploy the Worker on their own (`release-worker.yaml`) and
  don't trigger an app release (`exclude-paths` in `release-please-config.json`).

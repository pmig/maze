# Releasing Maze

The maintainer checklist for cutting a release, written for the first one
(`0.1.0-alpha.1`). The process is designed in PLAN §4.15 and §4.16 and
[ADR-011](adr/0011-release-process.md); [CONTRIBUTING.md](../CONTRIBUTING.md#releases)
has the short version.

```
PR (conventional title) ──squash──▶ main ──▶ ci.yaml (lint, test, build; nothing published)
                                       └──▶ release-please.yaml: opens/updates the release PR
                                            (version.txt + CHANGELOG.md)
release PR merged ──▶ tag 0.1.0-alpha.1 + GitHub Release (prerelease)
                  ──▶ release-please.yaml starts release-artifacts.yaml on the tag:
                      build, sign, notarize, upload to the release,
                      publish manifests to releases.mazemail.dev (Workers KV)
```

The `.dmg` and `.tar.zst` downloads live on the GitHub Release. Only the small signed
update manifests are served from `releases.mazemail.dev`, by the `releases`
Cloudflare Worker in [`releases/`](../releases) out of a Workers KV namespace
([ADR-025](adr/0025-update-manifests-on-cloudflare.md)).

Commits that only touch `website/` or `releases/` (`exclude-paths` in
`release-please-config.json`) don't appear in the changelog and don't trigger a
release; the site and the Worker deploy on their own (`website.yaml`,
`release-worker.yaml`).

## 1. One-time repository setup

### Settings

- **General → Pull Requests:** allow only **squash merging**, with *Default commit
  message* set to **Pull request title** (or *title and description*). release-please
  reads the squashed commits on `main`.
- **Branches → `main`:** require the `CI` checks and the `Semantic PR title` check.
- **Pages:** off. The manifests used to be served by GitHub Pages from a `gh-pages`
  branch; they now come from the Worker below. Keep `mazemail.dev` under your
  account's *Settings → Pages → Verified domains*, so nobody else can claim a
  subdomain on GitHub Pages.
- **Environment `releases`:** holds the Cloudflare secrets for `publish-manifests` and
  the Worker deploy, and must allow release tags (see below).

### Secrets (Settings → Secrets and variables → Actions)

| Secret                       | What it is                                                                                     | Used by                    |
| ---------------------------- | ---------------------------------------------------------------------------------------------- | -------------------------- |
| `UPDATE_SIGNING_KEY`         | The content of `update-key.secret` (see step 2), verbatim                                      | update manifests           |
| `CLOUDFLARE_API_TOKEN`       | one per **environment**: `manifests` (KV only), `releases` (deploys the Worker), `website` (the site); see below | `publish-manifests`, `release-worker.yaml` |
| `CLOUDFLARE_ACCOUNT_ID`      | **repository** secret (shared with the website): the Cloudflare account ID                     | `publish-manifests`, `release-worker.yaml` |
| `APPLE_CERTIFICATE_P12`      | base64 of the **Developer ID Application** certificate with its private key, exported as `.p12` | `sign-and-notarize-mac`    |
| `APPLE_CERTIFICATE_PASSWORD` | the password of that `.p12`                                                                    | `sign-and-notarize-mac`    |
| `APPLE_TEAM_ID`              | the 10-character Apple Team ID                                                                 | signing, entitlements      |
| `APPLE_NOTARY_KEY`           | the content of an App Store Connect API key (`AuthKey_….p8`) with the *Developer* role          | `notarytool`               |
| `APPLE_NOTARY_KEY_ID`        | that key's ID                                                                                  | `notarytool`               |
| `APPLE_NOTARY_ISSUER`        | the issuer ID of your App Store Connect API keys                                               | `notarytool`               |
| `APPLE_PROVISIONING_PROFILE` | base64 of a **Developer ID** provisioning profile for the App ID `dev.mazemail.maze` with the keychain sharing capability (it grants `keychain-access-groups`, `<TeamID>.dev.mazemail.maze`) | `bundle-mac --provisioning-profile` |

```sh
base64 -i DeveloperID.p12 | gh secret set APPLE_CERTIFICATE_P12
gh secret set APPLE_NOTARY_KEY < AuthKey_ABC123XYZ.p8
base64 -i Maze_Developer_ID.provisionprofile | gh secret set APPLE_PROVISIONING_PROFILE
gh secret set UPDATE_SIGNING_KEY < update-key.secret
```

What happens when secrets are missing:

- Without **all seven** Apple secrets, the macOS bundles are built and uploaded
  **unsigned**, and **no macOS update manifest** is published (the updater rejects
  bundles that aren't signed by our Team ID). The workflow warns but doesn't fail.
- Without `UPDATE_SIGNING_KEY`, no manifests are published at all.
- Without the Cloudflare secrets (or with the placeholder KV namespace id still in
  `releases/wrangler.jsonc`), `publish-manifests` fails after the assets are uploaded;
  fix it and re-run the failed job.

### releases.mazemail.dev (the `releases` Worker)

Do this once, in this order. The zone `mazemail.dev` is already on Cloudflare (see
[website/README.md](../website/README.md#one-time-cloudflare-setup)). Between steps 5 and
6 `releases.mazemail.dev` doesn't resolve for a minute; the updater treats that as "no
update right now" and tries again later.

1. **Log in locally** (OAuth with your own account, enough for everything below):

   ```sh
   cd releases
   mise run install
   wrangler login
   ```

2. **The KV namespace** `manifests` exists (created 2026-10-30) and
   its id is in [`releases/wrangler.jsonc`](../releases/wrangler.jsonc). Only for a new
   Cloudflare account: create it again and replace the id there (copy the id; don't let
   wrangler edit the file, it would add a second binding):

   ```sh
   wrangler kv namespace create manifests
   # → "id": "0123456789abcdef0123456789abcdef"
   wrangler kv namespace list   # shows it again
   ```

   Commit that (`chore(releases): set the KV namespace id`).
3. **Copy existing manifests over**, only if GitHub Pages already served some (skip it
   before the first release):

   ```sh
   git clone --depth 1 --branch gh-pages https://github.com/pmig/maze.git /tmp/gh-pages
   for f in /tmp/gh-pages/{stable,preview}/*.json; do
     key=${f#/tmp/gh-pages/}
     wrangler kv key put --binding MANIFESTS --remote "$key" --path "$f"
   done
   wrangler kv key list --binding MANIFESTS --remote
   ```

4. **Turn GitHub Pages off:** *Settings → Pages*, remove the custom domain
   `releases.mazemail.dev`, then unpublish the site (or `gh api -X DELETE
   repos/pmig/maze/pages`). The `gh-pages` branch can be deleted afterwards
   (`git push origin --delete gh-pages`).
5. **Delete the DNS record** `releases.mazemail.dev CNAME pmig.github.io` in the
   Cloudflare dashboard (*mazemail.dev → DNS → Records*). A Workers custom domain can't
   be created over an existing CNAME.
6. **Deploy the Worker once** from your machine. This creates the Worker
   `releases`, the DNS record and the certificate for `releases.mazemail.dev`
   (`routes` → `custom_domain`):

   ```sh
   mise run deploy        # wrangler deploy
   curl -sI https://releases.mazemail.dev/preview/macos-aarch64.json   # 200 or 404
   curl -sI -X POST https://releases.mazemail.dev/preview/macos-aarch64.json   # 405
   ```

7. **Two API tokens for GitHub Actions** (*My Profile → API Tokens → Create Token →
   Custom token*, *Account resources* = your account), one per job, so each can only do
   its own work:
   - **"manifests (GitHub Actions)"**: **Account · Workers KV Storage · Edit** only
     (reads and writes the manifests).
   - **"releases (GitHub Actions)"**: **Account · Workers Scripts · Edit** and **Zone ·
     Workers Routes · Edit** with *Zone resources* = `mazemail.dev` only (deploys the
     Worker and its custom domain). The KV binding needs no KV permission to deploy.
8. **Two GitHub environments**, each with its own token (the account ID is a repository
   secret, shared with the website):
   - **`manifests`**: release **tags** only (`publish-manifests` runs on the tag).
   - **`releases`**: the **`main`** branch only (Worker deploys).

   ```sh
   for env in manifests releases; do
     gh api -X PUT repos/pmig/maze/environments/$env --input - <<'JSON'
   {"deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}}
   JSON
   done
   gh api -X POST repos/pmig/maze/environments/manifests/deployment-branch-policies -f name='*' -f type=tag
   gh api -X POST repos/pmig/maze/environments/releases/deployment-branch-policies -f name=main -f type=branch
   gh secret set CLOUDFLARE_API_TOKEN --env manifests   # the KV-only token
   gh secret set CLOUDFLARE_API_TOKEN --env releases    # the deploy token
   ```

   Each environment has its own `CLOUDFLARE_API_TOKEN` (`website`: the site's token),
   so a job only gets the token of its environment. Once all three are set, delete a
   repository-level `CLOUDFLARE_API_TOKEN` (`gh secret delete CLOUDFLARE_API_TOKEN`):
   jobs without an environment then get no Cloudflare token at all.

   Don't add required reviewers: the release would wait for an approval before its
   manifests go out.

From then on a merge to `main` that touches `releases/` deploys the Worker
(`release-worker.yaml`), and every release writes its manifests to KV. The Worker
answers `GET`/`HEAD` for `/<stable|preview>/<macos|linux>-<aarch64|x86_64>.json` with
the KV value (`Content-Type: application/json`, `Cache-Control: public, max-age=60`),
`404` for everything else and `405` for other methods. It keeps no logs (observability
off) and has no `*.workers.dev` or preview URL.

## 2. The update signing key

Do this once, before the first release, because the public key is compiled into the
binary.

```sh
cargo xtask gen-update-key --out-dir ~/maze-update-key
# private key: ~/maze-update-key/update-key.secret  (mode 0600)
# public key:  ~/maze-update-key/update-key.pub
```

1. Store `update-key.secret` as the secret `UPDATE_SIGNING_KEY` and keep an offline
   backup in a password manager. Then delete the local copy.
2. Add the line from `update-key.pub` to
   [`crates/auto_update/keys/update_public_keys.txt`](../crates/auto_update/keys/update_public_keys.txt)
   in a PR (`chore(auto_update): add the update signing key`) and merge it **before**
   the release PR. While that file is empty, release builds can't verify manifests and
   the updater stays disabled.
3. To rotate later: add the new key as a second line, ship a release signed with the old
   key, then switch the secret to the new key and remove the old line.

### Check the updater locally

`cargo xtask update-e2e` (macOS, a few minutes) runs the update end to end without real
signing: it generates a throwaway key, builds two bundles of Maze with it
(`0.0.1-e2e.1` and `0.0.1-e2e.2`, preview channel, in `target/update-e2e`), serves a
signed manifest and the newer `.app.tar.zst` from a local server, lets the older bundle
update itself (`mazemail --check-update-once`, debug builds only, code signature check
skipped) and checks the swap, the kept previous version and a rollback. `--keep` keeps
the work directory; debug builds of the app also take `MAZEMAIL_UPDATE_BASE_URL` and
`MAZEMAIL_UPDATE_SKIP_CODESIGN=1` to try the title bar flow against such a server.

## 3. Cut the release

1. **Check the version.** release-please keeps a PR named like
   `chore(main): release 0.1.0-alpha.1` open. With `bump-patch-for-minor-pre-major` and
   the starting version `0.0.0`, its first proposal may not be `0.1.0-alpha.1`. If it
   isn't, pin it: add `"release-as": "0.1.0-alpha.1"` to the `"."` package in
   `release-please-config.json` in a `chore:` PR, and remove it again after the
   release. (A `Release-As: 0.1.0-alpha.1` footer in a squashed commit's body works
   too.)
2. **Read the release PR.** `version.txt` must say `0.1.0-alpha.1`, and `CHANGELOG.md`
   should read well; edit the PR if needed. release-please opens it with the built-in
   `GITHUB_TOKEN`, and a PR opened that way starts no workflows, so the required `CI`
   and `Semantic PR title` checks don't run on their own: **close and reopen the PR**
   to start them.
3. **Merge the release PR.** release-please creates the tag `0.1.0-alpha.1` (no `v`)
   and a GitHub **prerelease**, which puts it on the `preview` channel, then starts
   `Release artifacts` on the tag (a tag created with `GITHUB_TOKEN` doesn't trigger
   it by itself). If that step ever fails, start it by hand:
   `gh workflow run release-artifacts.yaml --ref refs/tags/0.1.0-alpha.1`.
4. **Watch `Release artifacts`** (`gh run watch`):
   - `Determine channel`: the tag matches `version.txt`, the channel is `preview`, and
     the signing secrets are complete (check its warnings).
   - `Build`: macOS aarch64 and x86_64 (bundle, sign, notarize, staple, `.dmg` and
     `.app.tar.zst`) and Linux x86_64 and aarch64 (`.tar.zst`), each uploaded to the
     release.
   - `Publish update manifests`: reads the published manifests from KV, writes
     `preview/<os>-<arch>.json`, signed, and puts the changed ones into KV with one
     `wrangler kv bulk put`, last, so no manifest ever points at a missing file.
5. **If a job fails**, fix the cause and re-run the failed jobs (`gh run rerun --failed`).
   Uploads use `--clobber`, and manifests are only published when every build
   succeeded.

## 4. Smoke test

On a Mac that never had Maze (or after removing it and its keychain item):

- [ ] The release has eight assets: `Maze-0.1.0-alpha.1-macos-{aarch64,x86_64}.{dmg,app.tar.zst}`
      and `Maze-0.1.0-alpha.1-linux-{x86_64,aarch64}.tar.zst`.
- [ ] `https://releases.mazemail.dev/preview/macos-aarch64.json` (and the other three)
      loads over HTTPS, says `"version": "0.1.0-alpha.1"`, and its `sha256` matches
      `shasum -a 256 Maze-0.1.0-alpha.1-macos-aarch64.app.tar.zst`.
- [ ] The `.dmg` opens with a window showing Maze and the Applications link; drag Maze
      to Applications.
- [ ] `spctl --assess --type execute -vv /Applications/Maze.app` says *accepted*,
      *source=Notarized Developer ID*; `codesign -dv /Applications/Maze.app` shows our
      Team ID; `xcrun stapler validate /Applications/Maze.app` succeeds.
- [ ] Maze opens without a Gatekeeper warning; the Dock shows the Maze icon and the menu
      bar says **Maze** (About Maze, Hide Maze, Quit Maze).
- [ ] `defaults read /Applications/Maze.app/Contents/Info.plist` shows
      `CFBundleIdentifier = dev.mazemail.maze`, `MazeVersion = 0.1.0-alpha.1` and the
      `mailto` URL scheme.
- [ ] First launch creates the keychain item "Maze storage key" and
      `~/Library/Application Support/mazemail/` with `mail/` inside. After adding an
      account and restarting, Keychain Access shows no other Maze item (service
      `mazemail`), and a start asks the keychain at most once (ADR-017).
- [ ] `/Applications/Maze.app/Contents/MacOS/mazemail --version` prints
      `mazemail 0.1.0-alpha.1`; `mazemail status` shows the app running.
- [ ] `eval "$(mazemail login --profile triage)"` shows the approval dialog; after
      *Allow once*, `mazemail import <some.mbox> --account <id>` (approve again)
      uploads it into a new folder of an IMAP test account, and after the next sync
      `mazemail search <word>` and `mazemail show <id>` find and render the mail.
- [ ] Importing a Thunderbird profile brings over its IMAP accounts (POP3 accounts and
      Local Folders are listed as skipped), and an IMAP account syncs.
- [ ] Quitting Maze (`cmd-q`) ends the process and removes `mazemail.sock`; the next
      start opens the same mail without asking for anything.
- [ ] Linux: `tar --zstd -xf Maze-0.1.0-alpha.1-linux-x86_64.tar.zst -C ~/.local` and
      `~/.local/mazemail.app/bin/mazemail --version`; the app starts and stores its key
      in the Secret Service.

## 5. After the release

- Remove a temporary `release-as` from `release-please-config.json`.
- Edit the GitHub Release notes if the generated changelog needs context (what works,
  what doesn't, known issues).
- A broken release: mark the GitHub Release as such and put the previous release's
  manifests back into KV (below), so the updater stops offering it; fix forward with
  the next release.

## 6. Inspecting and rolling back manifests

The KV namespace is the only copy of the published manifests; KV keys are the URL paths
without the leading slash. From `releases/` after `wrangler login` (or with
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` set):

```sh
cd releases
wrangler kv key list --binding MANIFESTS --remote
wrangler kv key get --binding MANIFESTS --remote preview/macos-aarch64.json
curl -s https://releases.mazemail.dev/preview/macos-aarch64.json   # what clients get
```

To roll a channel back, put a manifest of an older release back for every platform.
Either keep a copy before each release (`wrangler kv key get … >
preview-macos-aarch64.json`), or regenerate the previous release's manifests from the
repository root with its assets and the signing key (from your password manager):

```sh
gh release download 0.1.0-alpha.1 --dir /tmp/assets --pattern '*.tar.zst'
TAG=0.1.0-alpha.1 CHANNEL=preview COMMIT=<its commit, 7 chars> \
  KEY_FILE=/path/to/update-key.secret ASSET_DIR=/tmp/assets MANIFEST_DIR=/tmp/manifests \
  script/generate-manifests
```

Then, from `releases/`:

```sh
wrangler kv key put --binding MANIFESTS --remote preview/macos-aarch64.json --path /tmp/manifests/preview/macos-aarch64.json
# or all at once: [{"key": "preview/macos-aarch64.json", "value": "<manifest JSON>"}, ...]
wrangler kv bulk put --binding MANIFESTS --remote manifests.json
```

Clients see the change within a couple of minutes (KV propagation plus the 60-second
`Cache-Control`). An installed newer version is never downgraded: the updater ignores a
manifest older than what's running, so a rollback only stops new installs of the bad
release. The next release's `publish-manifests` compares against what is in KV, so
after a rollback it publishes normally. `wrangler kv key delete --binding MANIFESTS
--remote <key>` withdraws a platform completely (the updater then gets a 404).


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
                  ──▶ release-artifacts.yaml: build, sign, notarize, upload, publish manifests
```

Commits that only touch `website/` (`exclude-paths` in `release-please-config.json`)
don't appear in the changelog and don't trigger a release; the site deploys on its own
(`website.yaml`).

## 1. One-time repository setup

### Settings

- **General → Pull Requests:** allow only **squash merging**, with *Default commit
  message* set to **Pull request title** (or *title and description*). release-please
  reads the squashed commits on `main`.
- **Branches → `main`:** require the `CI` checks and the `Semantic PR title` check.
- **Pages:** *Deploy from a branch*, branch **`gh-pages`**, folder `/` (the branch is
  created by the first `publish-manifests` run; you can set the source afterwards).
  Custom domain **`releases.mazemail.dev`**, **Enforce HTTPS**. The workflow writes the
  `CNAME` file and `.nojekyll` itself.
- **DNS:** a `CNAME` record `releases.mazemail.dev → pmig.github.io`. Verify
  `mazemail.dev` under your account's *Settings → Pages → Verified domains*, so nobody
  else can claim the subdomain.

### Secrets (Settings → Secrets and variables → Actions)

| Secret                       | What it is                                                                                     | Used by                    |
| ---------------------------- | ---------------------------------------------------------------------------------------------- | -------------------------- |
| `RELEASE_BOT_TOKEN`          | A bot token (fine-grained PAT of a bot account, or a GitHub App token) with **Contents: read and write** and **Pull requests: read and write** on `pmig/maze`. Not `GITHUB_TOKEN`: tags created with it don't trigger `release-artifacts.yaml` | release-please, push to `gh-pages` |
| `UPDATE_SIGNING_KEY`         | The content of `update-key.secret` (see step 2), verbatim                                      | update manifests           |
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
- Without `RELEASE_BOT_TOKEN`, release-please can't open the release PR.

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
   should read well; edit the PR if needed.
3. **Merge the release PR.** release-please creates the tag `0.1.0-alpha.1` (no `v`)
   and a GitHub **prerelease**, which puts it on the `preview` channel.
4. **Watch `Release artifacts`** (`gh run watch`):
   - `Determine channel`: the tag matches `version.txt`, the channel is `preview`, and
     the signing secrets are complete (check its warnings).
   - `Build`: macOS aarch64 and x86_64 (bundle, sign, notarize, staple, `.dmg` and
     `.app.tar.zst`) and Linux x86_64 and aarch64 (`.tar.zst`), each uploaded to the
     release.
   - `Publish update manifests`: writes `preview/<os>-<arch>.json`, signed, to
     `gh-pages`, last, so no manifest ever points at a missing file.
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
- A broken release: mark the GitHub Release as such and revert the manifest commit on
  `gh-pages`, so the updater stops offering it; fix forward with the next release.

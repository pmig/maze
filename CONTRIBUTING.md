# Contributing to Maze

Thanks for helping with Maze, a local-first email client written in Rust. Read
[AGENTS.md](AGENTS.md) first. It has the naming rules (product "Maze", identifiers
`mazemail`), the crate layering and the code conventions, and it applies to humans
and coding agents alike.

## Toolchain

The toolchain is managed with [mise](https://mise.jdx.dev). There's no Homebrew step.

```sh
mise install        # Rust 1.99.1 (rust-toolchain.toml), cargo-nextest, cargo-deny, cargo-insta
mise run ci         # everything CI runs: lint, test, deny
```

Single tasks: `mise run fmt`, `mise run lint` (rustfmt check + clippy with `-D warnings`),
`mise run test` (nextest), `mise run deny` (licenses and advisories).

- macOS: Xcode Command Line Tools.
- Linux: the GPUI system packages. On Debian and Ubuntu, `script/linux-deps` installs them.

Run `mise run ci` before you commit.

### Keychain prompts in dev builds (macOS)

A dev build reads one keychain item at startup, its storage key (ADR-017). Rust signs
every build ad hoc with a new signature, so macOS asks again after each rebuild and
"Always Allow" doesn't stick. To make it stick, create a code signing identity once:
Keychain Access → Certificate Assistant → Create a Certificate…, name
**Maze Development**, identity type *Self-Signed Root*, certificate type *Code
Signing*. `mise start` then signs the binary with it (`script/dev-codesign`; without
the identity it does nothing). Allow the storage key "Always" once more after the
switch, and allow `codesign` to use the identity's key.

## Branches and worktrees

The repository is checked out as a bare repo with one git worktree per branch, so
several branches build side by side without stashing:

```sh
git -C <repo> worktree add <name> -b <name>      # new workspace
git -C <repo> worktree remove <name>             # when you're done
```

Every worktree has its own `target/`, and a GPUI build takes several GB, so remove
worktrees you're finished with.

## Commits and pull requests

- Commits follow [Conventional Commits](https://www.conventionalcommits.org):
  `feat(store): …`, `fix(imap): …`, `docs: …`, `ci: …`, `chore: …`.
- **The pull request title must be a conventional commit too.** The `Semantic PR title`
  check enforces it. Allowed types are `feat`, `fix`, `perf`, `refactor`, `deps`,
  `docs`, `chore`, `build`, `ci`, `revert`, `style` and `test`.
- Pull requests are **squash-merged** with the PR title as the commit message, so each
  branch lands on `main` as a single conventional commit. release-please reads those
  commits to build the changelog and pick the next version.
- Add dependencies to `[workspace.dependencies]` in the root `Cargo.toml` and check their
  license against `deny.toml`.
- Architecture decisions are recorded in [`docs/adr/`](docs/adr/README.md). Copy
  `template.md` for a new one.

## Releases

Contributors don't cut releases. A commit to `main` only runs CI. release-please keeps
a release PR open that updates `version.txt` and `CHANGELOG.md`. When a maintainer merges
it, release-please creates the tag (without a `v`, for example `0.2.0-alpha.3`) and the
GitHub Release, and `release-artifacts.yaml` builds, signs, notarizes and publishes the
bundles and the signed update manifests. Prereleases (alpha, beta, rc) go to the
`preview` channel, final versions to `stable`. There are no nightly builds. Builds from
`main` are unsigned workflow artifacts kept for 7 days, only for manual testing.

## Distribution

- **macOS:** only the `.dmg` from GitHub Releases, followed by Maze's built-in
  auto-updater. There is deliberately **no Homebrew cask, tap or formula** and no other
  macOS package manager, because a package manager would compete with the updater for
  the same app bundle (ADR-010). Please don't open PRs that add one.
- **Linux:** a `.tar.zst` with `mazemail.app/{bin,share}`. Flatpak or distro packages
  are a possible later option, with the updater disabled by a build flag.

The scripts live in `script/`: `bundle-mac`, `package-mac`, `sign-and-notarize-mac`,
`bundle-linux`, `generate-manifests`, `semver-gt` and `dev-codesign`. They are plain bash and should
stay shellcheck-clean.

## Security

Don't report vulnerabilities in public issues. See [SECURITY.md](SECURITY.md).

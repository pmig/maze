# Security policy

## Reporting a vulnerability

Please report security problems **privately** to **philip@pmig.at**. Don't open a public
issue or pull request for them. Include what you found, how to reproduce it, the Maze
version (`mazemail --version`) and your platform. You'll get an answer as soon as
possible, and we'll agree on a disclosure date with you once a fix is ready.

Maze is in early development (alpha). Only the latest release on the `preview` or
`stable` channel gets fixes.

## What is in scope

Maze handles mail, credentials and OAuth tokens, so these are the areas where we most
want to hear from you:

- **Encryption at rest.** Everything under `<data_dir>/mail/` (database, blobs, caches,
  search index) is encrypted with a master key kept in the OS keychain
  (ADR-012, PLAN §4.17). Ways to read mail from the data directory without the key,
  to leak the key, or to make Maze write message content to disk in plaintext outside
  the documented exceptions are in scope.
- **Agent sandbox.** Agents Maze starts, and the terminal commands it runs for them,
  run in an OS sandbox (Seatbelt on macOS, bubblewrap and seccomp on Linux) that must
  not be able to read `<data_dir>/mail/`, other sessions or the rest of your home
  directory (ADR-013, PLAN §4.18). Sandbox escapes are in scope.
- **Local API and CLI.** Every request that touches mail on `mazemail.sock` needs an
  approved, scoped token (ADR-006, PLAN §4.11). Ways around tokens, approval or peer
  checks are in scope.
- **Updates.** Update manifests are signed with Ed25519 and artifacts are checked by
  SHA-256, and macOS bundles must carry our Team ID (PLAN §4.15). Anything that lets
  an attacker deliver code through the updater is in scope.
- **Mail rendering.** Sanitizer bypasses, remote content loading while it's blocked, and
  tracking-pixel leaks in reader mode and "Open in browser".

## Known limits (not vulnerabilities)

These follow from the threat model in PLAN §4.17 and §4.18:

- Encryption at rest doesn't protect against root, a process that can debug Maze, or
  someone you allowed into the keychain item.
- **Linux is weaker than macOS:** any process running as you can read an unlocked
  Secret Service collection and with it the key. There, encryption protects copies and
  backups, and the sandbox protects against agents Maze starts.
- Agents you start yourself outside Maze are not sandboxed by Maze. They only see mail
  through the CLI with a token you approved.
- Plaintext that Maze produces on purpose is out of scope: agent scratch directories,
  attachments you open in other apps, CLI output and `mazemail export` files you asked
  for, notification text, and the Thunderbird profile you import from.
- Agents can still reach the network by default, because they have to reach their model
  provider.

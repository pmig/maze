# The `mazemail` command line client

`mazemail` gives you, your scripts and AI agents access to your mail from a terminal:
search, read, export, import and triage. This page explains how it works and how it
signs in. The full command reference, with every option and the JSON output shapes,
is [AGENTS-cli.md](AGENTS-cli.md); it's generated from the same command registry the
CLI is built from, and `mazemail help agents` prints the same text.

## How it works

- **One program.** `mazemail` without arguments starts the app, `mazemail <command>`
  runs a command, and `mazemail --headless` runs Maze without a window. In the macOS
  bundle it's `Maze.app/Contents/MacOS/mazemail`, on Linux
  `mazemail.app/bin/mazemail`. It's updated together with the app.
- **A thin client.** A command never opens the mail store, so it never needs the
  storage key. It sends one request to the running Maze over a Unix socket,
  `mazemail.sock` in the data directory (mode 0600), and prints the answer. If the
  data directory's path is too long for a socket, Maze binds in a short private
  directory under `$TMPDIR` and writes that path to `<data_dir>/socket-path`, where the
  CLI finds it.
- **Text or JSON.** Every command prints a readable table, or stable JSON with
  `--json`. Ids are short and stable across restarts: `m_…` message, `t_…` thread,
  `f_…` folder, `a_…` account, `tk_…` token, `p_…` proposal.
- **The same directories as the app.** `MAZEMAIL_DATA_DIR` and `MAZEMAIL_CONFIG_DIR`
  select another installation, for example a demo store from
  `cargo xtask seed-demo`.

```sh
mazemail status                       # is Maze running, am I signed in?
mazemail search 'is:unread' --limit 10
mazemail show t_19bd --format md      # a thread as Markdown
mazemail folders --json | jq .
```

## Signing in: tokens

Every command that touches mail needs a **session token** in the environment variable
`MAZEMAIL_SESSION`. There is no tokenless access: otherwise any program running as you
could read your decrypted mail through the socket, and encryption at rest would be
pointless. Without a token only `mazemail help`, `mazemail status` and
`mazemail login` work.

Get a token with `mazemail login` (or its alias `mazemail token create`):

```sh
eval "$(mazemail login --profile read-only --ttl 8h)"
```

1. The CLI prints "waiting for approval in Maze…".
2. Maze shows a dialog with the program that asks (its path and its parent process,
   read from the socket's peer credentials), the profile, the scope and the lifetime,
   with **Allow once** and **Deny**. Closing the dialog denies. Unanswered requests
   time out after five minutes.
3. Approved, the CLI prints `export MAZEMAIL_SESSION=mazemail_s_…`, which `eval` sets
   in your shell, like `op signin` in the 1Password CLI.

Options:

| Option                  | Meaning                                                                  |
| ----------------------- | ------------------------------------------------------------------------ |
| `--profile <profile>`   | `read-only` (default), `triage` or `writer`, see below                   |
| `--ttl <duration>`      | lifetime, e.g. `30m`, `8h`, `7d`, `1h30m`; default `8h`, at most `30d`   |
| `--account <a_…>`       | limit the token to this account (repeatable)                             |
| `--folder <f_…>`        | limit the token to this folder (repeatable)                              |
| `--label <text>`        | a name shown in the token list and the audit log, e.g. `claude`          |
| `--allow-send`          | allow `mazemail send` for this token (every send still asks)             |

Some rules:

- **Tokens can't mint tokens.** `mazemail login` from a program that already holds a
  token still needs your approval, and the new token gets exactly what you approve.
- Accounts whose AI policy excludes agents are never in a token's scope.
- Anything outside a token's scope looks like it doesn't exist (`not found`), so a
  token can't probe other accounts.
- Maze keeps only a hash of each token. `mazemail token list` shows the tokens with
  their profile, scope and expiry; `mazemail token revoke tk_…` revokes one (your own
  always works, another one asks first).
- The server only accepts connections from your own user id, and every request goes
  into the encrypted audit log with the program's pid and path (never mail content).

## Profiles

The profile decides what a token may do. Each command either runs (**allow**), becomes
a proposal you review in Maze (**propose**), waits for your approval in Maze (**ask**),
or fails with exit code 3 (**deny**).

| Commands                                                      | read-only | triage  | writer |
| ------------------------------------------------------------- | --------- | ------- | ------ |
| `search`, `show`, `folders`, `tags`, `contacts`, `identities`, `token list` | allow | allow | allow |
| `draft new`, `draft edit`, `propose`                          | deny      | allow   | allow  |
| `archive`, `move`, `tag`, `flag`, `read`                      | deny      | propose | allow  |
| `import`, `export`, `rule add`, `token revoke` (other tokens) | deny      | ask     | ask    |
| `send`                                                        | deny      | ask with `--allow-send` | ask with `--allow-send` |

Permanent delete isn't available to tokens at all. `mazemail help` lists every command
with what your current token may do with it.

## Approvals

Commands marked **ask** show the same kind of dialog as a login, describing the command
(for an import, which path; for an export, the query and the target). `import` and
`export` always ask, because an import uploads mail to your server (or adds accounts
from a Thunderbird profile) and an export writes plaintext outside the encrypted store.

```sh
eval "$(mazemail login --profile triage --label import)"
mazemail import ~/old-mail.mbox --account a_…          # asks in Maze
mazemail export 'in:Inbox after:2026-01-01' --to inbox.mbox       # asks in Maze
```

## Headless mode

`mazemail --headless` opens the store and serves the socket without a window, for
scripts, servers and tests. It runs until you press Ctrl-C (or send SIGTERM), then lets
running requests finish, removes the socket and closes the store.

Without a window, approvals are asked on the terminal Maze was started from:

```
Maze: /usr/local/bin/mazemail (pid 4242), started by /bin/zsh (pid 4100) asks to sign in to Maze with profile read-only, all accounts, for 8h.
Allow? [y/N]
```

Type `y` to approve. If headless Maze has no terminal (started by launchd or systemd),
every login and every command that needs approval is denied. Tokens created earlier
keep working.

Only one Maze (app or headless) can open a data directory at a time.

## Exit codes

| Code | Meaning                              |
| ---- | ------------------------------------ |
| 0    | ok                                   |
| 1    | other failure                        |
| 2    | usage error                          |
| 3    | permission denied                    |
| 4    | approval denied                      |
| 5    | Maze is not running                  |
| 6    | not signed in (no or expired token)  |

## What works in this version

`status`, `help`, `login`, `token`, `search`, `show`, `folders`, `tags`, `contacts`,
`identities`, `archive`, `move`, `tag`, `flag`, `read`, `propose`, `import`,
`export`, `account list`, `account add` (with `--smtp-host`, `--smtp-port` and
`--smtp-security` for the server Maze sends through), `account remove` and `sync` work.
Writing and sending mail works in the app; the CLI commands for it (`draft new`,
`draft edit`, `send`) and `rule add` and `attachment save` fail with "not available in
this version" (exit code 1). Proposals (from `propose` and from a triage token's
`archive`, `move`, `tag`, `flag` and `read`) are stored, but the review view in Maze
that accepts or rejects them isn't there yet, so nothing changes for now.

## Agents

Agents you run yourself (Claude Code, Codex and others in a terminal) use the CLI like
you do: give them a token, preferably `read-only` or `triage`, with a short lifetime and
a `--label`, and point them at [AGENTS-cli.md](AGENTS-cli.md) (or the output of
`mazemail help agents`). Agents that Maze starts itself, in its agent column and inside
a sandbox, will get a token for their session automatically; that part is planned, see
[security.md](security.md#agents).

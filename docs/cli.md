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
pointless. Without a token only `mazemail help`, `mazemail status`, `mazemail login`
and `mazemail open` work. `open` reads nothing: it shows a new message for a
`mailto:` link (what your system runs when you click one), which you review and send
yourself.

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
- A token covers every account unless it was created for some (`--account`).
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
| `search`, `show`, `folders`, `tags`, `contacts`, `identities`, `token list`, `rule list`, `rule show`, `saved-search list` | allow | allow | allow |
| `draft new`, `draft edit`, `propose`, `saved-search add`, `saved-search rename`, `saved-search edit` | deny | allow | allow |
| `archive`, `move`, `tag`, `flag`, `read`                      | deny      | propose | allow  |
| `import`, `export`, `rule add`, `rule enable`, `rule disable`, `rule delete`, `rule run`, `token revoke` (other tokens), `saved-search delete` | deny | ask | ask |
| `send`                                                        | deny      | ask with `--allow-send` | ask with `--allow-send` |

Permanent delete isn't available to tokens at all. `mazemail help` lists every command
with what your current token may do with it.

## Approvals

Commands marked **ask** show the same kind of dialog as a login, describing the command
(for an import, which path and what it brings in; for an export, the query and the
target). `import` and `export` always ask, because an import uploads mail to your server
(or adds accounts from a Thunderbird profile) and an export writes plaintext outside the
encrypted store.

```sh
eval "$(mazemail login --profile triage --label import)"
mazemail import ~/old-mail.mbox --account a_…          # asks in Maze
mazemail export 'in:Inbox after:2026-01-01' --to inbox.mbox       # asks in Maze
```

A Thunderbird import takes the whole profile unless `--only` names the parts, separated
by commas: `accounts`, `contacts` (the address books), `filters`, `saved-searches` and
`tags`. The dialog then says "Imports: only contacts and filters from …".

```sh
mazemail import ~/.thunderbird/abcd.default-release --only contacts,filters
```

Filters belong to accounts: imported on their own, they go to the accounts an earlier
import of the same profile created. Filters whose account isn't in Maze yet are
skipped (the result says so); import `--only accounts` first.

## Rules

Message rules (filters) run on new mail in the Inbox, in their order; see
[getting-started.md](getting-started.md#rules). Filters imported from Thunderbird
arrive disabled:

```sh
mazemail rule list                       # every rule, in order, with its problems
mazemail rule show r_b                   # one rule, with the import's note
mazemail rule run --folder f_k2 r_b      # try a rule on a folder first
mazemail rule enable r_b                 # it runs on new mail from now on
mazemail rule add --name Receipts --when 'from:receipts@shop.example' \
  --then tag:Receipts --then archive     # a new rule, enabled
```

`rule add --when` takes `from:`, `to:`, `cc:`, `subject:`, `list:`, `has:attachment` and
free text (matched against the message text), each negated with `-`; all terms must
match, or any of them with `OR`. Actions (`--then`, repeatable) are `read`, `flag`,
`tag:<name>`, `copy:<folder-id>`, `move:<folder-id>`, `archive`, `trash` and `stop`. A
rule with a move or copy belongs to that folder's account; otherwise pass `--account`
(a rule for every account needs a token for every account). Enabling, running, adding
and deleting rules change mail, so they always ask for your approval in Maze; listing
them is a read. What a rule does is in the audit log with the origin `rule:<id>`.

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

## Saved searches

Saved searches are the named searches Maze's folder pane lists under Saved Searches
(see [getting-started.md](getting-started.md#saved-searches)). Manage them by name or
id (`s_…`):

```sh
mazemail saved-search list
mazemail saved-search add Invoices 'invoice has:attachment'
mazemail saved-search rename Invoices Bills
mazemail saved-search edit Bills 'invoice OR receipt has:attachment'
mazemail saved-search delete Bills                  # asks in Maze
mazemail search --saved Invoices                    # run one
mazemail search --saved Invoices 'after:2026-01-01' # a query narrows it
```

Adding, renaming and editing change only what Maze shows, never mail, so a triage token
may do them; deleting asks. Every token sees every saved search's name and query, but
`search --saved` only returns mail in the token's scope, like any search. Maze's folder
pane shows the changes at once.

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
`export`, `rule list|show|add|enable|disable|delete|run`, `account list`, `account add` (with `--smtp-host`, `--smtp-port` and
`--smtp-security` for the server Maze sends through, and `--tenant` for the
organization of a `--oauth microsoft` sign-in), `account set` (a new name, password or
servers, or `--sign-in` again), `account remove`, `sync` and `saved-search` work.
Writing and sending mail works in the app; the CLI commands for it (`draft new`,
`draft edit`, `send`) and `attachment save` fail with "not available in this version"
(exit code 1), and so does a `propose` batch with an `unsubscribe` item.
Proposals (from `propose` and from a triage token's `archive`, `move`, `tag`, `flag`
and `read`) show at the top of Maze's agent column (`cmd-?`), where you accept or
reject each item; nothing changes until you accept. See
[agents.md](agents.md#proposals).

## Agents

Agents you run yourself (Claude Code, Codex and others in a terminal) use the CLI like
you do: give them a token, preferably `read-only` or `triage`, with a short lifetime and
a `--label`, and point them at [AGENTS-cli.md](AGENTS-cli.md) (or the output of
`mazemail help agents`). Agents that Maze starts itself, in its agent column and inside
a sandbox, get a token for their session automatically (`MAZEMAIL_SESSION`), revoked
when the session ends; see
[agents.md](agents.md) and [security.md](security.md#agents).

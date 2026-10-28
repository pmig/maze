# Maze

**The agentic email client.** Crafted for speed with humans and AI in mind.

An open source, local-first email client written in Rust. Built like
[Zed](https://zed.dev) (GPU-rendered, fast, small, agent-native) and behaving like
[Thunderbird](https://www.thunderbird.net) (same layout, same shortcuts).

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/screenshots/app-real-data-dark.png">
  <img alt="Maze with a demo mailbox: folders, the message list, a message and the agent column" src="docs/screenshots/app-real-data.png">
</picture>

- **Local-first.** Your mail lives on your computer in one store. Opening a folder,
  scrolling through 100,000 messages or searching never waits for the server.
- **Crafted for speed.** A native GPU-rendered window (GPUI, Zed's UI framework), no web
  view for the UI. A cold start shows the message list in well under a second.
- **Nothing to relearn.** Thunderbird's panes, folders, Quick Filter, global search and
  keyboard shortcuts, so moving over from Thunderbird doesn't mean relearning anything.
- **Encrypted at rest.** The database, message files and search index are encrypted
  with a key that stays in the macOS keychain (or the Secret Service on Linux). A copy of
  your data directory is useless without it.
- **Agents next to your mail.** Opt-in: the agent column runs Claude Code, Codex,
  Gemini CLI, Cursor, pi or your own agent over ACP, in an operating system sandbox,
  and their changes become proposals you accept or reject.
- **Scriptable.** The `mazemail` command line client gives scripts and agents scoped,
  approved access to your mail: search, read, export, propose changes for you to
  review. No token, no mail.

The app is called **Maze**. Every technical identifier (crate, binary, CLI, data
directories, environment variables) is **`mazemail`**, because the `maze` crate name is
taken.

## Status

**Pre-alpha.** Maze reads, writes, searches and triages mail and runs agents next to
it, but it isn't a daily driver yet. Expect rough edges. Maze keeps all mail in your
IMAP accounts, so another mail client keeps working alongside it.

Works today:

- the encrypted store (SQLCipher database, encrypted message files and search index)
- IMAP, Gmail and Microsoft 365 accounts: the Add Account dialog or
  `mazemail account add`, with password or browser sign-in (Gmail with your own OAuth
  client ID)
- IMAP sync, including CONDSTORE/QRESYNC delta sync, IDLE and Gmail labels, of your
  subscribed folders
- import of Thunderbird profiles (accounts, identities, tags, filters, address books),
  and of mbox files and Maildir directories, uploaded into a folder of an IMAP account
  (Maze is IMAP only: no POP3 and no Local Folders, see
  [ADR-019](docs/adr/0019-imap-only.md))
- search with Gmail-style operators (`from:`, `has:attachment`, `after:` …) and
  Quick Filter
- reading mail in a sanitized reader view, or as the sender designed it in the original
  HTML view (an embedded Servo engine with scripts off and every load going through
  Maze; macOS builds only), remote images only when you allow them (**Load images**,
  **Always for this sender**), `cmd-alt-o` to switch
- DKIM/SPF/DMARC results (SPF and DMARC as the receiving server recorded them), the
  sender's mail client, and link targets on hover with a warning for deceptive links
- the folder pane with Favorites and each account's special folders first, the message
  list as cards or a table sorted by any column, multi-select, Quick Move (`shift-m`)
  and right-click menus
- archive, move, delete (`backspace`; in Trash permanently, after asking), tag, flag
  and mark read, with undo (`cmd-z`), and a sync button (`cmd-shift-t`)
- tabs and splits like Zed's: mail views, single messages and drafts side by side,
  dragged into splits, restored after a restart, with the agent column staying right
- writing mail in a tab (Markdown, sent as plain text + HTML), each draft named with
  two random words, replies and forwards, local drafts, sending over SMTP (password or
  OAuth) with an outbox that never sends twice and a copy in Sent, `r` / `shift-r` to
  reply, a footer from the settings and an optional "Sent with Maze" line
- AI agents in the agent column, off until you share an account with them: Claude Code,
  Codex, Gemini CLI, Cursor, pi and custom ACP agents, installed and kept up to date by
  Maze, signed in from the column, with model, mode and thinking pickers, sandboxed
  (Seatbelt on macOS, bubblewrap on Linux), their changes reviewed as proposals
- editing `settings.json` in a tab (`cmd-,`), applied on save, with its problems listed
  by line; Zed themes from `~/.config/mazemail/themes`
- the `mazemail` CLI and `mazemail --headless` for scripts and agents
- updates in the background with a "Restart to update" button, and a rollback if a new
  version keeps failing to start

Not there yet:

- drafts on the server, forwarding attachments, send later, undo send, address
  autocomplete, spell check, signatures that switch with the identity
- filter rules (imported Thunderbird filters are kept, disabled)
- notifications for new mail
- a settings window with forms (settings are edited as JSON for now), showing the
  recovery key
- Linux packaging (Flatpak) and the original HTML view on Linux

## Install

### macOS

Download `Maze-<version>-macos-aarch64.dmg` (Apple Silicon) or `…-x86_64.dmg` (Intel)
from [GitHub Releases](https://github.com/pmig/maze/releases) once the first
release is published, open it and drag Maze to Applications. From then on Maze updates
itself: it installs new versions in the background and shows **Restart to update** in
the title bar ([details](docs/getting-started.md#updates)). There is deliberately no
Homebrew cask (see [ADR-010](docs/adr/0010-auto-update.md)).

### Linux

Download `Maze-<version>-linux-<arch>.tar.zst` from the same page and unpack it to
`~/.local/`:

```sh
tar --zstd -xf Maze-*-linux-x86_64.tar.zst -C ~/.local
~/.local/mazemail.app/bin/mazemail
```

### Build from source

The toolchain is managed with [mise](https://mise.jdx.dev). On macOS you also need the
Xcode Command Line Tools; on Debian and Ubuntu, `script/linux-deps` installs the system
packages.

```sh
mise install          # Rust 1.99.1, cargo-nextest, cargo-deny, cargo-insta
mise start            # starts a dev build of Maze, with its own data directory
mise run start:lite   # the same without the HTML engine (Servo): much faster first build
script/bundle-mac     # builds dist/mac-<arch>/Maze.app, the .dmg and the .app.tar.zst
```

Want to look around without your own mail? `MAZEMAIL_DEMO=1 mise start` shows a
synthetic mailbox, and `cargo xtask seed-demo --data-dir /tmp/maze-demo` creates a
real (encrypted) store with demo mail that the CLI can talk to.

## Quick start

1. **Start Maze.** On first launch it creates the storage key in your keychain and an
   empty store.
2. **Sign in the CLI.** The CLI talks to the running app and needs a token you approve
   in Maze:

   ```sh
   eval "$(mazemail login --profile triage --ttl 8h)"
   ```

   Inside the app bundle the CLI is `/Applications/Maze.app/Contents/MacOS/mazemail`;
   link it into your `PATH` or use an alias.
3. **Add an account.** Use **Add Account…** in the folder pane, or the CLI with a
   writer token (`mazemail account add you@example.com`, password from the terminal or
   `--password-stdin`; `--oauth google` or `--oauth microsoft` for the browser
   sign-in). Gmail works with an app password; the browser sign-in for Gmail and
   Microsoft needs your own OAuth client ID for now, see
   [Getting started](docs/getting-started.md#accounts). Or import your
   Thunderbird profile, which takes its IMAP accounts over, with identities, tags,
   address books and filters:

   ```sh
   mazemail import ~/Library/Thunderbird/Profiles/abcd1234.default-release --format thunderbird
   ```

4. **Import mail files.** An mbox file or a Maildir is uploaded into a folder of one
   of your accounts (a folder named after the file unless you pass `--folder`). Maze
   asks you to approve the import:

   ```sh
   mazemail status                      # lists your accounts and their ids
   mazemail import ~/old-mail.mbox --account a_… --folder Archive/2019
   ```

5. **Search and read.**

   ```sh
   mazemail search 'from:anna has:attachment after:2026-09-01'
   mazemail show t_19bd
   ```

More in [docs/getting-started.md](docs/getting-started.md) and
[docs/cli.md](docs/cli.md).

## Where your data lives

| What                                          | macOS                                    | Linux                     |
| --------------------------------------------- | ---------------------------------------- | ------------------------- |
| Mail store (encrypted), socket, logs          | `~/Library/Application Support/mazemail` | `$XDG_DATA_HOME/mazemail` |
| `settings.json` (no mail content, no addresses) | `~/.config/mazemail`                   | `~/.config/mazemail`      |
| Storage key (Maze's only keychain item)       | keychain                                 | Secret Service            |
| Account passwords and OAuth refresh tokens    | in the encrypted mail store              | in the encrypted mail store |

Development builds use `mazemail-dev` and a separate keychain item, so they never touch
your real store. `MAZEMAIL_DATA_DIR` and `MAZEMAIL_CONFIG_DIR` override the
directories.

## Security in short

- Everything with mail content (database, message files, caches, search index) is
  encrypted with a random 256-bit key that never leaves the keychain. Account passwords
  and OAuth refresh tokens are in the encrypted database too, so Maze keeps one
  keychain item and reads it once per launch.
- The CLI never opens the store. It asks the running app over a Unix socket (mode
  0600), and every request that touches mail needs a token you approved, scoped to a
  profile (read-only, triage or writer), accounts and a lifetime.
- On purpose in plaintext: what you export, what the CLI prints for an approved token,
  the thread you attach to an agent conversation (while the session runs), and the
  Thunderbird profile you imported from.
- Agents that Maze starts run in a sandbox that can't read the store or your home
  folder; they reach mail only through the CLI, with a token for the accounts you
  shared.
- Linux is weaker than macOS: any program running as you can read an unlocked Secret
  Service collection, and with it the key.

The details are in [docs/security.md](docs/security.md). Report vulnerabilities
privately, see [SECURITY.md](SECURITY.md).

## Documentation

- [Getting started](docs/getting-started.md): install, first launch, import, accounts,
  keyboard shortcuts
- [The `mazemail` CLI](docs/cli.md): tokens, profiles, approvals and `--headless`
- [CLI reference for agents](docs/AGENTS-cli.md), generated from the command registry
- [Agents](docs/agents.md): the agent column, installing Claude Code, Codex, Gemini,
  Cursor and pi, sign-in, the sandbox, custom agents
- [Security](docs/security.md): encryption at rest, keys and recovery, API tokens
- [Releasing](docs/releasing.md): the maintainer checklist

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) and [AGENTS.md](AGENTS.md) (naming, crate
layering, conventions). Architecture decisions are in [docs/adr/](docs/adr/README.md).
Run `mise run ci` before you commit.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE).

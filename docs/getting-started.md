# Getting started with Maze

Maze is a local-first email client: your mail is stored on your computer, encrypted,
and the app behaves like Thunderbird. This guide covers installing, the first launch,
importing mail, accounts and the keyboard shortcuts.

Maze is **pre-alpha**. Reading, searching and triaging work; writing and sending mail
don't yet. Keep your mail on the server.

## Install

### macOS (Apple Silicon or Intel, macOS 13 or later)

1. Download `Maze-<version>-macos-aarch64.dmg` (Apple Silicon) or
   `Maze-<version>-macos-x86_64.dmg` (Intel) from
   [GitHub Releases](https://github.com/pmig/mazemail/releases).
2. Open the `.dmg` and drag **Maze** onto **Applications**.
3. Start Maze from Applications or Spotlight.

Release builds are signed with a Developer ID and notarized by Apple. If you download a
build that isn't (for example a CI artifact), macOS refuses to open it the first time:
try to open it once, then click **Open Anyway** in System Settings → Privacy & Security.
Builds you make yourself aren't affected.

Maze is distributed only as this `.dmg`, followed by its own updater. There is
deliberately no Homebrew cask or other package manager, because it would fight the
updater over the same app ([ADR-010](adr/0010-auto-update.md)). The updater isn't part
of the app yet; until it is, install new versions the same way.

### Linux

Download `Maze-<version>-linux-x86_64.tar.zst` (or `-aarch64`) and unpack it:

```sh
tar --zstd -xf Maze-*-linux-x86_64.tar.zst -C ~/.local
~/.local/mazemail.app/bin/mazemail
```

`mazemail.app/share/` holds a desktop entry (`dev.mazemail.maze.desktop`, also the
`mailto:` handler) and the icon; copy them to `~/.local/share/` to get Maze into your
launcher. On Linux, Maze keeps its key in the Secret Service (GNOME Keyring, KWallet),
which must be running.

### From source

```sh
git clone https://github.com/pmig/mazemail && cd mazemail
mise install            # the pinned Rust toolchain and the test tools
cargo run --release     # starts Maze
script/bundle-mac       # optional: dist/mac-<arch>/Maze.app, .dmg and .app.tar.zst
```

A build from source is a **dev** build: it uses its own data directory
(`mazemail-dev`), its own keychain item and the bundle ID `dev.mazemail.maze-dev`, so it
never touches the store of an installed Maze.

## The command line client

The CLI is the same program as the app. In the macOS bundle it's
`Maze.app/Contents/MacOS/mazemail`; put it on your `PATH`, for example:

```sh
ln -s /Applications/Maze.app/Contents/MacOS/mazemail ~/.local/bin/mazemail
```

On Linux it's `~/.local/mazemail.app/bin/mazemail`. See [cli.md](cli.md) for how it
signs in.

## First launch

- The window opens immediately and shows "Opening Maze…" while the key is read and the
  store opened in the background.
- On the very first launch Maze creates a random storage key, saves it in your keychain
  (macOS: the login keychain; Linux: the Secret Service) and creates an empty,
  encrypted store in the data directory:

  | Platform | Data directory                           | Settings                              |
  | -------- | ---------------------------------------- | ------------------------------------- |
  | macOS    | `~/Library/Application Support/mazemail` | `~/.config/mazemail/settings.json`    |
  | Linux    | `$XDG_DATA_HOME/mazemail`                | `~/.config/mazemail/settings.json`    |

- Only one Maze can use a data directory. If you start a second one, it tells you
  that Maze is already running and quits.
- If Maze finds encrypted mail but no key (for example after restoring a backup on a
  new Mac), it asks for your **recovery key** or offers to **start fresh and re-sync**.
  It never deletes the old data; see [security.md](security.md#lost-key-and-recovery).

### Just looking around

To try Maze without your own mail, start it with synthetic mail and no store at all:

```sh
MAZEMAIL_DEMO=1 /Applications/Maze.app/Contents/MacOS/mazemail
```

From a source checkout, `cargo xtask seed-demo --data-dir /tmp/maze-demo` creates a
real, encrypted store with 400 demo messages that the CLI can talk to; it prints the
environment variables to start Maze on it.

### Settings

`settings.json` only needs the keys you want to change; Maze merges it over its
defaults and picks up changes while it runs. The defaults, with a comment for every
key, are in [`assets/settings/default.jsonc`](../assets/settings/default.jsonc). For
example:

```jsonc
{
  "theme": { "mode": "dark" },
  "thread_list": { "view": "table" },
  "reader": { "remote_content": "never" }
}
```

Accounts, identities and SMTP servers are stored in the encrypted database, never in
`settings.json`.

## Import mail

Maze imports mbox files, Maildir directories and whole Thunderbird profiles. Importing
goes through the CLI and needs a token with the **triage** or **writer** profile, and
Maze asks you to approve every import:

```sh
eval "$(mazemail login --profile triage)"     # approve in Maze

mazemail import ~/Downloads/old-mail.mbox      # into Local Folders
mazemail import ~/Mail/archive --format maildir
mazemail import ~/Library/Thunderbird/Profiles/abcd1234.default-release --format thunderbird
```

The format is detected when you leave out `--format`. `--account a_…` imports into a
specific account instead of Local Folders (`mazemail status` lists the accounts and
their ids).

From a Thunderbird profile Maze takes over:

- accounts with their server settings, identities, signatures and SMTP servers
- Local Folders (and POP3 mail stores) as local accounts
- tags, saved searches (`virtualFolders.dat`) and address books
- message filters, imported as **disabled** rules (rules don't run yet)

Running the import again doesn't create duplicates. Nothing in the profile is changed,
so Thunderbird keeps working side by side. Passwords aren't imported: accounts that
need one, or a new OAuth sign-in, are listed in the import result.

Remember that the Thunderbird profile itself still holds all your mail in plaintext.

## Accounts

Maze talks IMAP to your mail servers. It uses CONDSTORE and QRESYNC where the server
supports them (only changes are fetched), IDLE for new mail, and Gmail's extensions for
labels and threads. Which message bodies are kept offline is set with `"offline"` in
`settings.json` (`headers_only`, `recent` with `offline_recent_days`, `all` or
`on_demand`), per account under `"accounts"`.

Today, accounts come into Maze through the Thunderbird import above. Passwords and
OAuth refresh tokens are stored in the keychain, never in the database or settings.

### Gmail and Google Workspace: bring your own OAuth client ID

Thunderbird's OAuth client IDs belong to Mozilla and can't be reused, and Google
requires a verification for the full-mail scope (`https://mail.google.com/`) that Maze
doesn't have yet. Until then you use your own Google Cloud OAuth client, which is
common practice for open source mail clients:

1. In the [Google Cloud console](https://console.cloud.google.com/), create a project.
2. Configure the **OAuth consent screen**: user type *External* (or *Internal* for a
   Workspace domain), leave it in *Testing* and add your own address as a test user.
3. Under **Credentials**, create an **OAuth client ID** of type **Desktop app**.
4. Put the client ID and secret into `~/.config/mazemail/settings.json`:

   ```jsonc
   {
     "auth": {
       "google": {
         "client_id": "1234567890-abc.apps.googleusercontent.com",
         "client_secret": "GOCSPX-…"
       }
     }
   }
   ```

Signing in opens Google's page in your browser and returns to Maze through a loopback
address (`http://127.0.0.1:<port>`), the same flow Thunderbird uses, with PKCE. If
Google rejects the client, check these two values.

### Microsoft 365 and Outlook.com

Maze signs in to Microsoft with OAuth2 (IMAP and SMTP scopes) through the browser, like
Thunderbird. Many company tenants don't allow apps an administrator hasn't approved.
Then sign-in fails with a message to forward to your IT department: an administrator
has to grant admin consent for Maze in Microsoft Entra ID under *Enterprise
applications*, or assign you to it. Sign in again once they have.

## Searching

- **Quick Filter** (`cmd-shift-k`) filters the current folder as you type.
- **Search all messages** (`cmd-k`) searches every account.

Both understand free text plus Gmail-style operators, for example
`from:anna has:attachment after:2026-09-01`, `is:unread tag:todo`,
`subject:"offsite" -from:noreply`. The same queries work with `mazemail search`.

## Keyboard shortcuts

Maze uses Thunderbird's shortcuts and fills the gaps with Zed's. These are bound today
(macOS; on Linux `cmd` is `ctrl`):

| Action                               | Key                         |
| ------------------------------------ | --------------------------- |
| Command palette                      | `cmd-shift-p`               |
| Search all messages                  | `cmd-k`                     |
| Quick Filter                         | `cmd-shift-k`               |
| Toggle folder pane                   | `cmd-b`                     |
| Toggle message pane                  | `F8`                        |
| Toggle agent column                  | `cmd-?`                     |
| Next / previous message              | `f` / `b`, `down` / `up`    |
| Next / previous unread               | `n` / `p`                   |
| First / last message, page down / up | `home` / `end`, `pagedown` / `pageup` |
| Toggle read                          | `m`                         |
| Star                                 | `s`                         |
| Archive                              | `a`                         |
| Hide Maze / hide others / minimize   | `cmd-h` / `cmd-alt-h` / `cmd-m` |
| Quit                                 | `cmd-q`                     |

Single-letter shortcuts work when the message list or a message has focus, never inside
a text field. Reply (`cmd-r`), reply all (`cmd-shift-r`) and forward (`cmd-l`) are bound
but only show a notice until the composer lands. The rest of Thunderbird's keymap
(tags `1`–`9`, junk, delete, threads, history) follows as the features arrive.

## Getting help

- `mazemail --help` and `mazemail help <command>` for the CLI
- [GitHub issues](https://github.com/pmig/mazemail/issues) for bugs and questions
- Security problems privately to philip@pmig.at, see [SECURITY.md](../SECURITY.md)

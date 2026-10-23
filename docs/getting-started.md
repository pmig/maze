# Getting started with Maze

Maze is a local-first email client: your mail is stored on your computer, encrypted,
and the app behaves like Thunderbird. This guide covers installing, the first launch,
importing mail, accounts, updates and the keyboard shortcuts.

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

Maze is distributed only as this `.dmg`, followed by its own [updater](#updates). There
is deliberately no Homebrew cask or other package manager, because it would fight the
updater over the same app ([ADR-010](adr/0010-auto-update.md)). Keep Maze in
Applications (or another folder you can write to), so it can update itself.

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
mise start              # starts Maze
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

### The message list

The list shows two-line cards or, a click (or `cmd-alt-t`) away, Thunderbird's table
with the columns Star · Subject · Correspondents · Date. Correspondents is the sender
of mail you received and the recipients, with a small arrow, of mail you sent. More
columns (Attachment, Unread, Size, Tags, Account, Sender, Recipients) come from the
columns button in the list header or by right-clicking a column header; drag a header's
edge to resize it, and click **Date** to switch between newest and oldest first.

Threads are on by default: a thread is one row with its message count; `→` or a click
on the count opens its messages as a reply tree, `←` closes it, `*` opens all threads
and `\` closes them, `cmd-shift-a` selects a whole thread. The threads button (or
`cmd-alt-g`) turns threads off and lists every message on its own.

`shift-down` / `shift-up` (or shift-click) select several rows, cmd-click adds or
removes one, `cmd-a` selects them all. Archive, Quick Move, toggle read and star then
act on every selected row at once, and the message pane says how many are selected.

**Quick Move** (`shift-m`), like Thunderbird's Quick Folder Move: a small picker lists
the folders of the selected mail's account (`Archives / 2026`). Type part of a folder's
name to filter, pick one with the arrow keys and press `enter` to move the selection
there; `escape` cancels. With an empty filter the folders you moved mail to last come
first. Moves stay within an account.

Cards or table, threads, sort order, columns and widths are the same in every folder.
Maze remembers them in `ui_state.json` in the data directory, together with your recent
Quick Move folders; until you change them, `thread_list.view`, `thread_list.threaded`
and `thread_list.sort` in `settings.json` apply.

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

### Adding an account

- **In the app:** **Add Account…** in the folder pane (or the button in an empty
  Maze, or "Maze: Add Account" in the command palette). Enter your address and
  password for any IMAP server, or use **Sign in with Google** / **Sign in with
  Microsoft**. The server is detected for well-known providers (Gmail, Outlook.com,
  iCloud, Yahoo, Fastmail, GMX, …); **Server settings…** lets you set host, port,
  security and user name yourself. Maze connects, logs in and lists the folders before
  it saves anything, so a wrong password or an unreachable server keeps the dialog open
  with the error.
- **From the command line** (needs a **writer** token, and you approve every account
  in Maze):

  ```sh
  eval "$(mazemail login --profile writer)"

  mazemail account add anna@example.com                    # asks for the password
  security find-generic-password -s mail -w | mazemail account add anna@example.com --password-stdin
  mazemail account add anna@company.com --oauth microsoft  # browser sign-in
  mazemail account add anna@gmail.com --oauth google       # needs auth.google.client_id
  mazemail account add me@example.org --host mail.example.org --port 143 --security starttls

  mazemail account list      # accounts and their sync state
  mazemail sync              # look for new mail now
  mazemail account remove a_…
  ```

  The password is read from the terminal without echo, or with `--password-stdin` from
  the first line of stdin; it is never a command-line argument. `--name`, `--host`,
  `--port`, `--security` (`tls`, `starttls`, `none`) and `--username` override the
  detected settings.
- **From Thunderbird:** the [Thunderbird import](#import-mail) takes over your
  accounts with their server settings. Passwords aren't imported; Maze lists the
  accounts that still need one.

Passwords and OAuth refresh tokens are stored in the keychain, never in the database or
settings. Removing an account deletes its local mail and keychain items; nothing changes
on the server.

On servers that keep track of subscriptions, Maze syncs only your **subscribed**
folders, plus the Inbox and the special folders (Sent, Drafts, Trash, Junk, Archive),
like Thunderbird. Subscribe to a folder on the server's webmail or in another client to
get it into Maze. Servers that report no subscriptions at all, and Gmail (whose labels
are folders), get every folder synced.

### Gmail and Google Workspace

The quickest way is an **app password**: turn on 2-Step Verification for the Google
account, create an app password at <https://myaccount.google.com/apppasswords> and
enter it in **Add Account** with your Gmail address instead of your normal password.
Maze detects the server; **Test Connection** checks the login before you add the
account. Workspace domains whose administrator turned app passwords off need the
browser sign-in below.

#### Sign in with Google: bring your own OAuth client ID

Thunderbird's OAuth client IDs belong to Mozilla and can't be reused, and Google
requires a verification for the full-mail scope (`https://mail.google.com/`) that Maze
doesn't have yet. Until then you use your own Google Cloud OAuth client, which is
common practice for open source mail clients:

1. In the [Google Cloud console](https://console.cloud.google.com/), create a project.
2. Configure the **OAuth consent screen**: user type *External* (or *Internal* for a
   Workspace domain), leave it in *Testing* and add your own address as a test user.
3. Under **Credentials**, create an **OAuth client ID** of type **Desktop app**.
4. Put the client ID and secret into `~/.config/mazemail/settings.json` (Maze reads
   them as `auth.google.client_id` and `auth.google.client_secret`):

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

Development builds read `~/.config/mazemail-dev/settings.json` instead.

Signing in opens Google's page in your browser and returns to Maze through a loopback
address (`http://127.0.0.1:<port>`), the same flow Thunderbird uses, with PKCE. If
Google rejects the client, check these two values.

### Microsoft 365 and Outlook.com

Microsoft doesn't accept passwords over IMAP any more, so Outlook.com, Hotmail and
Microsoft 365 accounts always use **Sign in with Microsoft**: OAuth2 with the IMAP and
SMTP scopes in the browser, like Thunderbird. Maze has no Microsoft client ID of its own
yet, so you register one (free, a few minutes):

1. In the [Microsoft Entra admin center](https://entra.microsoft.com/), open
   **App registrations** and choose **New registration**. Any Microsoft account works,
   including a personal Outlook.com one.
2. Name it (say, "Maze"). Under **Supported account types**, choose *Accounts in any
   organizational directory and personal Microsoft accounts*.
3. Under **Authentication**, add the platform **Mobile and desktop applications** with
   the redirect URI `http://127.0.0.1`. Maze listens on a random port; Microsoft
   ignores the port for loopback addresses.
4. Under **API permissions**, add the delegated permissions `IMAP.AccessAsUser.All`,
   `SMTP.Send` and `offline_access` (in *APIs my organization uses*, search for
   *Office 365 Exchange Online*, or use *Microsoft Graph* for `offline_access`).
5. Copy the **Application (client) ID** from the app's overview page into
   `~/.config/mazemail/settings.json` (Maze reads it as `auth.microsoft.client_id`).
   No client secret is needed:

   ```jsonc
   {
     "auth": {
       "microsoft": {
         "client_id": "00000000-0000-0000-0000-000000000000"
       }
     }
   }
   ```

Then restart Maze and use **Sign in with Microsoft** in **Add Account**. Many company
tenants don't allow apps an administrator hasn't approved.
Then sign-in fails with a message to forward to your IT department: an administrator
has to grant admin consent for Maze in Microsoft Entra ID under *Enterprise
applications*, or assign you to it. Sign in again once they have.

## Updates

Maze updates itself like Zed. It checks for a new version every hour and after your
computer wakes from sleep, downloads and installs it in the background while you keep
working, and then shows a small **Restart to update** button in the title bar. Nothing
changes until you restart (or quit and start Maze again); afterwards a notice says
"Updated to x.y.z" with a link to the release notes. While a download runs, a small
progress circle sits in the same place; problems show as a warning icon whose tooltip
explains them.

- **Maze: Check for Updates** (in the command palette and the **Maze** menu) checks
  right away.
- Every update is verified before it's installed: the update manifest must be signed
  with Maze's key, the download must match its SHA-256, and on macOS the new app must be
  signed by the same Apple team. The new app replaces the old one in a single step, so
  there is never a half-installed Maze. The previous version is kept in
  `<data directory>/updates/previous/`.
- If a new version keeps failing to start (three launches in a row), Maze offers
  **Roll back to _previous version_** and restarts with the version it kept. Your mail
  stays as it is.
- If Maze runs from the `.dmg`, from Downloads (macOS runs such apps from a temporary
  read-only copy) or from a folder you can't write to, it can't replace itself. It then
  tells you about new versions with "Move Maze to Applications to get automatic
  updates" and offers the new `.dmg` instead.
- Builds from source (dev builds) never update themselves; "Check for Updates" says so.

The settings, with their defaults:

```jsonc
{
  "auto_update": true,                 // false: only check when you ask
  "update_channel": null,              // "stable" or "preview"; null: the channel of the installed build
  "auto_update_install": "background"  // "notify_only": show "Update to x.y.z" instead of installing
}
```

Alphas, betas and release candidates are published on the **preview** channel, final
versions on **stable**. Preview users also get a stable release when it's newer. Changes
to these settings apply while Maze runs.

On Linux the updater replaces `~/.local/mazemail.app` the same way.

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
| Move to a folder (Quick Move)        | `shift-m`                   |
| Select several rows / all rows       | `shift-down` / `shift-up`, `cmd-a` |
| Open / close a thread                | `right` / `left`            |
| Open / close all threads             | `*` / `\`                   |
| Select the whole thread              | `cmd-shift-a`               |
| Table view / threads on or off       | `cmd-alt-t` / `cmd-alt-g`   |
| Hide Maze / hide others / minimize   | `cmd-h` / `cmd-alt-h` / `cmd-m` |
| Quit                                 | `cmd-q`                     |

Single-letter shortcuts work when the message list or a message has focus, never inside
a text field. Reply (`cmd-r`), reply all (`cmd-shift-r`) and forward (`cmd-l`) are bound
but only show a notice until the composer lands. The rest of Thunderbird's keymap
(tags `1`–`9`, junk, delete, history) follows as the features arrive.

## Getting help

- `mazemail --help` and `mazemail help <command>` for the CLI
- [GitHub issues](https://github.com/pmig/mazemail/issues) for bugs and questions
- Security problems privately to philip@pmig.at, see [SECURITY.md](../SECURITY.md)

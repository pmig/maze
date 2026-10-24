# Getting started with Maze

Maze is a local-first email client: your mail is stored on your computer, encrypted,
and the app behaves like Thunderbird. This guide covers installing, the first launch,
importing mail, accounts, writing mail, updates and the keyboard shortcuts.

Maze is **pre-alpha**. Reading, searching and triaging work, and writing and sending
mail works in a first version (see [Writing mail](#writing-mail)). Keep your mail on the
server.

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

Press `cmd-,` (or choose **Maze → Settings…**, or "Maze: Open Settings" in the command
palette) to edit `settings.json` in a tab. Maze creates the file if it doesn't exist
yet. It's a plain JSON editor (comments and trailing commas are allowed):

- `cmd-s` saves and applies the settings at once. The tab shows a dot while it has
  unsaved changes, and closing it asks whether to save them.
- Problems are listed below the editor with their line: syntax errors, keys Maze
  doesn't know (ignored) and values of the wrong type (that key uses its default). A
  file with a syntax error changes nothing: Maze keeps the settings it had until you
  fix it.
- When the file changes on disk while the tab is open (Maze writes your favorite
  folders into it; you might edit it elsewhere too), the tab reloads it. If you have
  unsaved changes, it asks instead: **Reload** or **Keep Mine**.
- "Maze: Open Default Settings" (or the **default settings** link in the banner at
  the top of your settings) shows every available setting with its default and a
  comment, read-only. Copy the keys you want to change into your own file.

`settings.json` only needs the keys you want to change; Maze merges it over its
defaults and picks up changes while it runs, also when you edit it in another editor.
The defaults, with a comment for every key, are in
[`assets/settings/default.jsonc`](../assets/settings/default.jsonc). For example:

```jsonc
{
  "theme": { "mode": "dark" },
  "thread_list": { "view": "table" },
  "reader": { "remote_content": "never" }
}
```

Accounts, identities and SMTP servers are stored in the encrypted database, never in
`settings.json`.

### Themes

`"theme"` picks light or dark and the theme for each:

```jsonc
"theme": { "mode": "system", "light": "One Light", "dark": "One Dark" }
```

`"mode"` is `"system"` (follow macOS), `"light"` or `"dark"`. Maze reads themes in
Zed's format: besides the built-in One Light and One Dark, put Zed theme files
(`*.json`, e.g. from Zed's extension repository) into `~/.config/mazemail/themes/` and
name one of their themes in `"light"` or `"dark"`. Changes apply as soon as you save,
also to the settings editor's colors. An unknown name falls back to the built-in theme.

### Tabs and splits

The middle of the window works like Zed's editor area: tabs, and panes split side by
side or above each other, while the folder pane stays on the left and the agent column
on the right.

- The window starts with an **Inbox** tab: the message list and the message. Picking a
  folder opens it in that tab (or in the mail tab you used last). Right-click a folder
  and choose **Open in New Tab** to keep the current one open as well.
- **Double-click** a message (or press `enter` or `cmd-o`, or choose **Open in New Tab**
  in its menu) to open just that message in a tab of its own, full width, titled with
  its subject.
- **New messages, replies and forwards** open in a tab too. A new draft gets a random
  two-word name such as `brave-otter`, which stays its tab title (the subject is the
  tooltip) and is saved with the draft. A dot on the tab means unsaved changes; closing
  such a tab asks whether to save the draft. Sending closes the tab. **Composer: Open
  In New Window** in the command palette moves a draft into a window of its own.
- **Splits:** drag a tab onto the edge of a pane to split it (the drop area is
  highlighted), onto another pane's tab bar to move it there, or along a tab bar to
  reorder. `cmd-\` splits the active pane to the right with a copy of its tab
  (a draft can't be shown twice, so its split shows the selected folder);
  `cmd-ctrl-left` / `right` / `up` / `down` split in that direction. Drag the line
  between two panes to resize them. A pane whose last tab closes disappears.
- The button at the left of the title bar (or `cmd-b`) hides and shows the folder pane.

Maze remembers the tabs and splits in `ui_state.json` and opens them again on the next
start: which folder, message or draft each tab shows, never subjects or other mail
content. A tab whose message or draft no longer exists is left out.

### The folder pane

The folder pane on the left has three sections: **Favorites**, **Unified Folders** (the
Inbox, Sent, … of all accounts together) and **Accounts** with each account's folders.
Click a section's or an account's name to collapse or expand it.

Each account lists its special folders first, in a fixed order: Inbox, Drafts, Sent,
Archive, All Mail, Starred, Important, Junk and Trash, even where the server keeps them
deeper in its tree (Gmail's `[Gmail]/Sent Mail`, `INBOX.Trash`). A thin line separates
them from your other folders, which follow in the server's order.

Right-click a folder and choose **Add to Favorites** to list it under Favorites at the
top; **Remove from Favorites** (on the folder or on its favorite) takes it out again.
**Mail: Toggle Favorite Folder** in the command palette does the same for the open
folder. Favorites are sorted like the accounts and their folders; with more than one
account they are grouped under the account's name. A favorite opens its folder, with
the same unread count.

Favorites are saved in `settings.json` as the account's label and the folder's path,
so they survive restarts and re-syncs, and you can edit them by hand:

```jsonc
{
  "folder_pane": {
    "favorites": [
      { "account": "work", "folder": "Customers/Acme" }
    ]
  }
}
```

Maze keeps your comments and formatting when it changes the list. A favorite whose
folder no longer exists (or whose account was renamed) is hidden, not deleted.

### The message list

The list shows two-line cards or, a click (or `cmd-alt-t`) away, Thunderbird's table
with the columns Star · Subject · Correspondents · Date. Correspondents is the sender
of mail you received and the recipients, with a small arrow, of mail you sent. More
columns (Attachment, Unread, Size, Tags, Account, Sender, Recipients) come from the
columns button in the list header or by right-clicking a column header; drag a header's
edge to resize it.

Click any column header to sort by it, and again to reverse the order; an arrow shows
the sorted column. Dates and sizes start newest and largest first, the star, unread and
attachment columns put those messages first, text columns go A to Z. The sort button
in the list header (cards and table) and **Sort By…** in the command palette or the
View menu offer every order, including the received time. Subjects sort without `Re:`,
`Fwd:`, `AW:` or `WG:` and ignore case; people sort by name, else address; messages
without a subject or tags come last either way. A thread sorts by what its row shows:
its newest message's date, sender, recipients and size, its subject, whether any of its
messages is starred, unread or has attachments. Sorting a very large folder by anything
but the date takes a moment ("Sorting…"); the selected message stays selected.

Right-click a message (in the list, cards or table, or in the message pane) for a menu
with Reply, Reply All, Forward, Mark as Read or Unread, Star or Unstar, Archive, Move to
Folder… and Select Thread, each with its shortcut. Right-clicking a message that isn't
selected selects it (without marking it read); right-clicking inside a selection of
several messages keeps it, and the menu acts on all of them. The message pane's menu
also copies the sender's address.

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
  iCloud, Yahoo, Fastmail, GMX, …), and so is the SMTP server for sending (see
  [SMTP servers](#smtp-servers)); **Server settings…** lets you set host, port,
  security and user name of both yourself. Maze connects, logs in and lists the folders
  before it saves anything, so a wrong password or an unreachable server keeps the
  dialog open with the error. **Test Connection** checks the IMAP login and then the
  SMTP login, without saving or sending anything.
- **From the command line** (needs a **writer** token, and you approve every account
  in Maze):

  ```sh
  eval "$(mazemail login --profile writer)"

  mazemail account add anna@example.com                    # asks for the password
  security find-generic-password -s mail -w | mazemail account add anna@example.com --password-stdin
  mazemail account add anna@company.com --oauth microsoft  # browser sign-in
  mazemail account add anna@gmail.com --oauth google       # needs auth.google.client_id
  mazemail account add me@example.org --host mail.example.org --port 143 --security starttls
  mazemail account add me@example.org --smtp-host mail.example.org --smtp-port 465 --smtp-security tls

  mazemail account list      # accounts and their sync state
  mazemail sync              # look for new mail now
  mazemail account remove a_…
  ```

  The password is read from the terminal without echo, or with `--password-stdin` from
  the first line of stdin; it is never a command-line argument. `--name`, `--host`,
  `--port`, `--security` (`tls`, `starttls`, `none`) and `--username` override the
  detected settings; `--smtp-host`, `--smtp-port` and `--smtp-security` the SMTP
  server's.
- **From Thunderbird:** the [Thunderbird import](#import-mail) takes over your
  accounts with their server settings. Passwords aren't imported; Maze lists the
  accounts that still need one.

Passwords and OAuth refresh tokens are stored in Maze's encrypted database, under the
storage key, never in plaintext or in the settings. The storage key is Maze's only
keychain item, so starting Maze reads the keychain once, however many accounts you
have. Removing an account deletes its local mail and its saved password or sign-in;
nothing changes on the server.

Earlier versions kept every password and token as its own keychain item. The first
start of a newer Maze moves them into the encrypted database and deletes the items:
macOS may ask once more for each of them, and then never again. See
[security.md](security.md#what-is-encrypted).

On servers that keep track of subscriptions, Maze syncs only your **subscribed**
folders, plus the Inbox and the special folders (Sent, Drafts, Trash, Junk, Archive),
like Thunderbird. Subscribe to a folder on the server's webmail or in another client to
get it into Maze. Servers that report no subscriptions at all, and Gmail (whose labels
are folders), get every folder synced.

### SMTP servers

Every identity sends through an SMTP server with the account's login (its saved
password, or the Google/Microsoft sign-in). Maze sets it up by itself, for new
accounts and for accounts you added before Maze could send:

| Provider                                   | SMTP server                                    |
| ------------------------------------------ | ---------------------------------------------- |
| Gmail and Google Workspace                 | `smtp.gmail.com`, TLS on 465                   |
| Outlook.com and Microsoft 365              | `smtp.office365.com`, STARTTLS on 587          |
| iCloud, Yahoo, Fastmail, GMX, web.de, …    | the provider's (built-in list)                 |
| everything else                            | `smtp.<domain>` (or the IMAP host with `imap.` → `smtp.`), STARTTLS on 587 |

Use **Server settings…** in Add Account (or `--smtp-host` and friends) when your
provider uses something else. Maze never falls back to an unencrypted connection when
STARTTLS fails.

**Sent copies:** Gmail and Microsoft 365 file the copy of a sent message in your Sent
folder themselves, so Maze doesn't upload one. For every other server Maze uploads the
copy into the identity's Sent folder (`APPEND`) right after sending. This follows the
SMTP server's `saves_sent` setting (`auto`, `yes`, `no`), which is `auto` for servers
Maze sets up.

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

## Writing mail

Press `cmd-n`, click **+** in the title bar, or choose **Message → New Message**. The
draft opens in a tab of its own (see [Tabs and splits](#tabs-and-splits)), named with
two random words such as `brave-otter`; split the window to keep the Inbox next to it:

- **From** picks the identity you send as. Replies pick it for you: the identity whose
  address the message was sent to (To, Cc or `Delivered-To`), else the account's default.
- **To**, **Cc** and **Bcc** (click **Cc Bcc** or press `cmd-shift-c`) take addresses
  separated by commas: `anna@example.com, "Doe, John" <john@example.com>`.
- The body is **Markdown**: `**bold**`, `*italic*`, lists, `> quotes`, links. Maze sends
  your text as the plain-text part and renders it as the HTML part, so recipients see
  normal formatted mail. Single line breaks are kept.
- **Attach…** (`cmd-shift-a`) adds files; they are read when the message is sent.
- **"Sent with Maze"** (on by default) adds a short line below your messages:
  "Sent with Maze (https://mazemail.dev)", with a plain link to the website, no
  tracking. It's a small way to support Maze. It's a setting, not a switch in the
  draft: turn it off with `"compose": { "support_maze": false }` (or per account), and
  the preview under the body shows whether it's on.
- `cmd-enter` sends and closes the tab. Escape or closing the tab (`cmd-w`) asks
  whether to keep a draft when something changed.

A **footer** for every message you send, such as your company's details, goes into
`settings.json` (Markdown):

```jsonc
{
  "compose": {
    "footer": "Acme Inc. · Main Street 1, Vienna · [acme.example](https://acme.example)",
    "support_maze": true
  },
  // Per account (by its name): another footer, or none.
  "accounts": { "Private": { "footer": "", "support_maze": false } }
}
```

The footer and the Maze line are shown muted under the body while you write, but
they aren't part of the text you edit: Maze adds them when the message is sent, after
your text and signature. In a reply or forward they go above the quoted message (where
your own text ends), not at the very bottom below the quote. The plain-text part gets a
blank line, the footer, a blank line and the Maze line; the HTML part the rendered
footer and a small grey "Sent with Maze" line.

Reply (`cmd-r`), reply all (`cmd-shift-r`) and forward (`cmd-l`) open the same kind of
tab with the recipients, `Re:`/`Fwd:` subject and the quoted message filled in (HTML mail is
quoted as Markdown). Messages are sent right away. When the server can't be reached,
the message waits in the outbox and goes out by itself when it can; a message the server
refuses (a wrong password, a rejected recipient, Microsoft 365's *SendAsDenied* for an
address you aren't allowed to send from) keeps the draft open with the explanation.
Maze records every message the server accepted, so even a crash in the middle of sending
never sends it twice.

Drafts are saved on your computer (encrypted, like your mail) a moment after you stop
typing. Not there yet: drafts in the server's Drafts folder, forwarding the original's
attachments, signatures switching with the identity, spell check, address completion,
send later and undo send.

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
| Toggle folder pane                   | `cmd-b` (or the title bar's left button) |
| Toggle message pane                  | `F8`                        |
| Toggle agent column                  | `cmd-?`                     |
| New agent conversation (agent column) | `cmd-n`                    |
| Accept / reject all agent proposals  | `cmd-shift-enter` / `cmd-shift-backspace` |
| Accept / reject a proposal item      | `y` / `n` (review focused)  |
| Next / previous message              | `f` / `b`, `down` / `up`    |
| Next / previous unread               | `n` / `p`                   |
| First / last message, page down / up | `home` / `end`, `pagedown` / `pageup` |
| Toggle read                          | `m`                         |
| Star                                 | `s`                         |
| Archive                              | `a`                         |
| Move to a folder (Quick Move)        | `shift-m`                   |
| Undo the last archive, move or flag  | `cmd-z`                     |
| Select several rows / all rows       | `shift-down` / `shift-up`, `cmd-a` |
| Open / close a thread                | `right` / `left`            |
| Open / close all threads             | `*` / `\`                   |
| Select the whole thread              | `cmd-shift-a`               |
| Open the message in a tab            | `enter`, `cmd-o`, double-click |
| New message                          | `cmd-n`                     |
| Reply / reply all / forward          | `cmd-r` / `cmd-shift-r` / `cmd-l` |
| Send (draft)                         | `cmd-enter`                 |
| Close the tab                        | `cmd-w`                     |
| Next / previous tab                  | `ctrl-tab` / `ctrl-shift-tab`, `cmd-}` / `cmd-{` |
| Tab 1 … 8 / last tab                 | `cmd-1` … `cmd-8` / `cmd-9` |
| Split right                          | `cmd-\`                      |
| Split left / right / up / down       | `cmd-ctrl-left` / `right` / `up` / `down` |
| Focus the pane left / right / above / below | `cmd-alt-left` / `right` / `up` / `down` |
| Save draft / attach / Cc and Bcc     | `cmd-s` / `cmd-shift-a` / `cmd-shift-c` |
| Table view / threads on or off       | `cmd-alt-t` / `cmd-alt-g`   |
| Hide Maze / hide others / minimize   | `cmd-h` / `cmd-alt-h` / `cmd-m` |
| Quit                                 | `cmd-q`                     |

Single-letter shortcuts work when the message list or a message has focus, never inside
a text field; they act on the tab that has the focus.

Zed splits and moves between panes with `cmd-k` followed by an arrow key. In Maze
`cmd-k` is Thunderbird's search, and a two-key shortcut starting with `cmd-k` would make
search wait a second for the second key, so Maze uses `cmd-ctrl-<arrow>` and
`cmd-alt-<arrow>` instead. You can bind Zed's keys in your own `keymap.json`. The rest of Thunderbird's keymap (tags `1`–`9`, junk, delete, history)
follows as the features arrive.

## Agents

The fourth column runs coding agents (Claude Code, Codex, Gemini CLI, Cursor, pi or
your own) in a sandbox, with the selected thread as context. AI is off until you share
an account with agents (`"agent": { "accounts": { "Work": "any" } }`). See
[agents.md](agents.md).

## Getting help

- `mazemail --help` and `mazemail help <command>` for the CLI
- [GitHub issues](https://github.com/pmig/mazemail/issues) for bugs and questions
- Security problems privately to philip@pmig.at, see [SECURITY.md](../SECURITY.md)

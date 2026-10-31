# Getting started with Maze

Maze is a local-first email client: your mail is stored on your computer, encrypted,
and the app behaves like Thunderbird. This guide covers installing, the first launch,
accounts, importing mail, the window (tabs, folders, the message list, reading and
searching), writing mail, agents, settings, updates and the keyboard shortcuts.

Maze is **pre-alpha**. Reading, searching, triaging, writing and sending mail work, and
so do AI agents in the agent column once you turn them on, but expect rough edges.
Maze keeps all mail in your IMAP accounts, so another mail client keeps working
alongside it.

## Install

### macOS (Apple Silicon or Intel, macOS 13 or later)

1. Download `Maze-<version>-macos-aarch64.dmg` (Apple Silicon) or
   `Maze-<version>-macos-x86_64.dmg` (Intel) from
   [GitHub Releases](https://github.com/pmig/maze/releases).
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

`mazemail.app/share/` holds a desktop entry (`dev.mazemail.maze.desktop`) and the icon;
copy them to `~/.local/share/` to get Maze into your launcher. The desktop entry
registers Maze for `mailto:` links, but Maze doesn't open links handed to it by other
apps yet. On Linux, Maze keeps its key in the Secret Service (GNOME Keyring, KWallet),
which must be running, and runs agents in bubblewrap (`bwrap`), which must be installed
if you use them. Linux builds don't include the original HTML engine yet; they show
every message in the [reader](#reading-mail-reader-and-original).

### From source

```sh
git clone https://github.com/pmig/maze && cd maze
mise install            # the pinned Rust toolchain and the test tools
mise start              # starts Maze
script/bundle-mac       # optional: dist/mac-<arch>/Maze.app, .dmg and .app.tar.zst
```

`mise start` and `script/bundle-mac` include the original HTML engine (Servo, the
`original-html` feature), and so do the macOS release builds; a plain `cargo build`
leaves it out. Servo's build scripts need Python 3.11 or newer (`mise install` provides
it; `uv` works too), its first build takes about three minutes longer, and it downloads
a prebuilt SpiderMonkey library once. It builds into its own directory, `target/servo`, so
switching between `mise start` and tests or your editor's rust-analyzer doesn't rebuild
half the tree. `mise run start:lite` starts Maze without the engine (the original view
then falls back to the reader). For a local release build, `cargo build --profile
release-fast` uses thin LTO and is far quicker than the fat-LTO `release` profile.

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

## Accounts

Maze talks IMAP to your mail servers. It uses CONDSTORE and QRESYNC where the server
supports them (only changes are fetched), IDLE for new mail, and Gmail's extensions for
labels and threads. Which message bodies are kept offline is set with `"offline"` in
`settings.json` (`headers_only`, `recent` with `offline_recent_days`, `all` or
`on_demand`), per account under `"accounts"`. With `recent`, a message's age is the date
the message list shows (its `Date:` header), not when the server received it; any other
message's body is downloaded when you open it.

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

#### One organization only: the tenant

The *Supported account types* of the app registration decide who can sign in with it:

- **Multi-tenant** (*Accounts in any organizational directory and personal Microsoft
  accounts*, as in step 2): any work, school or personal account. Maze's default tenant,
  `common`, works.
- **Single-tenant** (*Accounts in this organizational directory only*): only accounts
  of the organization that registered the app. That's how an organization keeps its
  registration to itself. Sign-in through `common` fails with `AADSTS50194`, and Maze
  says: "This app registration only allows its own organization. Set
  `auth.microsoft.tenant` to your tenant ID or domain, or register the app for any
  organization."

For a single-tenant registration, set `auth.microsoft.tenant` to the organization's
**Directory (tenant) ID** (on the app's overview page, next to the client ID) or one of
its verified domains:

```jsonc
{
  "auth": {
    "microsoft": {
      "client_id": "00000000-0000-0000-0000-000000000000",
      "tenant": "contoso.com"
    }
  }
}
```

`auth.microsoft.tenant` accepts `common` (the default), `organizations` (work and school
accounts of any organization), `consumers` (personal accounts only), a tenant ID or a
domain; anything else is reported as a settings error and `common` is used. **Add
Account** shows the tenant under its fields when the address belongs to Microsoft (or
under **Server settings…**), filled in from the setting, so one account can use another
tenant; on the command line, `mazemail account add … --oauth microsoft --tenant
contoso.com` does the same. A tenant Microsoft doesn't know fails with "Microsoft
doesn't know the tenant contoso.com".

Each account keeps the tenant it signed in with and renews its sign-in there, so
changing `auth.microsoft.tenant` later only affects new sign-ins.

## Import mail

Maze keeps all mail in your server accounts: it speaks IMAP (including Gmail and
Microsoft 365) and has no POP3 and no Local Folders ([ADR-019](adr/0019-imap-only.md)).
It imports whole Thunderbird profiles, and uploads mbox files and Maildir directories
into a folder of one of your accounts. Importing goes through the CLI and needs a token
with the **triage** or **writer** profile, and Maze asks you to approve every import:

```sh
eval "$(mazemail login --profile triage)"     # approve in Maze

mazemail status                                # lists the accounts and their ids
mazemail import ~/Downloads/old-mail.mbox --account a_…
mazemail import ~/Mail/archive --format maildir --account a_… --folder Archive/2019
mazemail import ~/Library/Thunderbird/Profiles/abcd1234.default-release --format thunderbird
```

The format is detected when you leave out `--format`.

### mbox files and Maildir directories

`--account` picks the account; you can leave it out only when you have exactly one.
`--folder` is a path below the account, with `/` between the levels (`Archive/2019`);
a folder that doesn't exist yet is created on the server. Without `--folder` the mail
goes into a new top-level folder named after the file or directory. Maildir++
subfolders (`.Sent`, `.Archive.2024`) become subfolders of that folder.

Every message is uploaded to the server with IMAP `APPEND`, with its flags (read,
flagged, answered …) and tags. The uploads are queued like any other change and sent
by the sync engine, so an import works offline and is retried until the server has
everything; the messages appear in Maze after the folder's next sync. Running the
import again skips messages that are already in the target folder (by Message-ID, or by
date, sender and subject for messages without one) or still waiting to be uploaded.

### Thunderbird profiles

From a Thunderbird profile Maze takes over:

- IMAP accounts (Gmail and Microsoft 365 included) with their server settings,
  identities, signatures and SMTP servers
- tags, saved searches (`virtualFolders.dat`) and address books
- message filters, imported as **disabled** rules (rules don't run yet)

POP3 accounts and Local Folders are skipped, and the import result says so. For a POP3
account, enable IMAP at your provider and add the account in Maze. To keep mail from
Local Folders, move it into an IMAP folder in Thunderbird before importing, or import
its mbox files (`Mail/Local Folders/` in the profile) with
`mazemail import <file> --account …`.

Running the import again doesn't create duplicates. Nothing in the profile is changed,
so Thunderbird keeps working side by side. Passwords aren't imported: accounts that
need one, or a new OAuth sign-in, are listed in the import result.

Remember that the Thunderbird profile itself still holds all your mail in plaintext.

### Local Folders from earlier versions

Earlier versions of Maze imported into a local account called Local Folders. Empty
ones are removed when you update. Local Folders that still hold mail stay readable,
searchable and exportable, but they never sync, and a later version removes them. Maze
reminds you at every start. To keep that mail, export it and upload it into an account:

```sh
mazemail export 'account:"Local Folders"' --to local-folders.mbox
mazemail import local-folders.mbox --account a_… --folder "Local Folders"
```

## The Maze window

The window has Thunderbird's layout: the folder pane on the left, the message list and
the message in the middle, and the agent column on the right. The middle holds tabs
and splits like Zed's editor.

### Tabs and splits

The middle of the window works like Zed's editor area: tabs, and panes split side by
side or above each other.

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
the same unread count. Maze checks favorites for new mail every 30 seconds (other
folders every 2 minutes, the Inbox at once) and shows a
[notification](#notifications) for new mail in them, except in Sent.

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

**Go to Folder** (`cmd-k`, or "Mail: Go To Folder" in the command palette) opens any
folder without the mouse: type part of its name, pick it with the arrow keys and press
`enter`. It lists the unified folders, then your favorites, then every folder of every
account as its path ("Work / Archives / 2025"). The folders you opened from it last come
first. `arc25` finds "Archives / 2025": every word you type must appear in the path, in
order if not in one piece.

### The message list

The list shows two-line cards or, a click (or `cmd-alt-t`) away, Thunderbird's table
with the columns Star · Unread · Subject · Correspondents · Date. Correspondents is the
sender of mail you received and the recipients, with a small arrow, of mail you sent.
Unread mail is bold and has a blue dot, in the cards next to the sender and in the
table's Unread column; a collapsed thread has it while any of its messages is unread.
Click the dot (or the empty spot where a read message's dot would be) to mark that
message or thread read or unread without selecting it, like `m`; `cmd-z` undoes it.
More columns (Attachment, Size, Tags, Account, Sender, Recipients) come from the
columns button in the list header or by right-clicking a column header, which also
turns columns off; drag a header's edge to resize it.

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
with Reply, Reply All, Forward, Mark as Read or Unread, Star or Unstar, Archive, Report
Junk, Delete, Move to Folder… and Select Thread, each with its shortcut. Right-clicking a message that isn't
selected selects it (without marking it read); right-clicking inside a selection of
several messages keeps it, and the menu acts on all of them. The message pane's menu
also copies the sender's address.

Threads are on by default: a thread is one row with its message count; `→` or a click
on the count opens its messages as a reply tree, `←` closes it, `*` opens all threads
and `\` closes them, `cmd-shift-a` selects a whole thread. The threads button (or
`cmd-alt-g`) turns threads off and lists every message on its own.

`shift-down` / `shift-up` (or shift-click) select several rows, cmd-click adds or
removes one, `cmd-a` selects them all. Archive, Report Junk, Delete, Quick Move,
toggle read and star then act on every selected row at once, and the message pane says
how many are selected.

**Report Junk** (`j`, Thunderbird's key, or the **Junk** button next to **Archive** in
the message header) marks the mail as junk on your server (the `$Junk` keyword spam
filters learn from) and moves it to the account's Junk folder; on Gmail it goes to
Spam. An account without a Junk folder only gets the mark. `cmd-z` moves it back.

**Delete** (`backspace` or `delete`, also in the right-click menu) moves the selection to
the account's Trash; `cmd-z` brings it back. On Gmail that removes every label. In a
Trash folder, deleting removes the mail from the server for good, so Maze asks first.

**Quick Move** (`shift-m`), like Thunderbird's Quick Folder Move: a small picker lists
the folders of the selected mail's account (`Archives / 2026`). Type part of a folder's
name to filter, pick one with the arrow keys and press `enter` to move the selection
there; `escape` cancels. With an empty filter the folders you moved mail to last come
first. Moves stay within an account.

Cards or table, threads, sort order, columns and widths are the same in every folder.
Maze remembers them in `ui_state.json` in the data directory, together with your recent
Quick Move folders; until you change them, `thread_list.view`, `thread_list.threaded`
and `thread_list.sort` in `settings.json` apply.

### Reading mail: reader and original

The message header shows who sent the message and, when the message says so, the mail
client they wrote it with (`· via Thunderbird 128.3 (Linux)`; the tooltip shows the
header it came from). Below it is the sender check: the DKIM, SPF and DMARC results and
a verdict, **Verified sender**, **Not verified** or **Suspicious sender** (a DMARC
failure); hover it for the explanation. SPF and DMARC come from the results your
receiving server recorded in the message (`Authentication-Results`), for any IMAP
account.

HTML mail can be shown two ways, and the **Reader / Original** buttons in the message
header (or `cmd-alt-o`) switch the message you're looking at:

- **Reader** shows the text of the message in your theme: fast, quiet, the same font
  and colors for every sender, and good for most mail. Layout tables, columns and
  backgrounds are simplified.
- **Original** shows the message as its sender designed it (newsletters, receipts,
  Outlook tables), laid out by Maze's built-in engine. It shows the sanitized
  message: no scripts, forms or embedded frames, and nothing is loaded from the
  network unless you allow remote images (below). Links work as in the reader (see
  [Links in a message](#links-in-a-message)), with the same target preview. Select text and
  `cmd-c` copies it.

The message pane next to the list uses the reader by default; a message you open in a
tab of its own (double-click, `enter` or `cmd-o`) shows the original. Change the
defaults with `reader.default_mode` (the pane) and `reader.tab_mode` (tabs) in
`settings.json`: `"reader"` or `"original"`. Plain-text mail always uses the reader.
A build without the `original-html` feature (a plain `cargo build`, and the Linux
release builds for now) shows the reader and says so.

**Remote images.** Pictures that are attached to a message (inline images) always
show. Images a message loads from the web can tell the sender that, when and from which
IP address you opened it, so `reader.remote_content` decides: `"ask"` (the default)
blocks them and shows a slim banner with how many were blocked and how many tracking
pixels Maze removed; **Load images** loads them for this message, **Always for this
sender** remembers the sender (in the encrypted database) and loads them from then on.
`"never"` blocks them without asking, even for remembered senders; `"always"` loads
them. Each account can override it (`"accounts": { "work": { "remote_content":
"never" } }`). Maze downloads the images itself, never the engine, and known tracking
pixels are dropped either way.

### Attachments

Attachments are listed under the message header with their size. Click one to open it
in the app your system uses for its type; the arrow next to it (**Save…**) asks where to
save it, starting in Downloads. Large messages may only have their text downloaded so
far: Maze then downloads the whole message first (the size shows "…" meanwhile), which
needs a connection to the server.

An attachment you open is written to a private folder only you can read
(`$XDG_RUNTIME_DIR/mazemail/attachments` on Linux, the system's temporary folder on
macOS), read-only, so save your changes elsewhere. Maze deletes these copies when it
quits, and after a day if it didn't quit cleanly. Programs and scripts (`.exe`, `.app`,
`.sh`, `.command`, …) only open after Maze asks you first.

### Links in a message

Hover a link in a message to see where it really goes: like a browser, Maze shows the
link's target (not its text) in a small box at the bottom left of the message, and hides
it when the pointer leaves the link.

- A `mailto:` link shows the address it writes to.
- A long address is shortened in the middle, so the host stays readable
  (`https://billing.example.com/acc…/invoices/2026-10.pdf`). A user name before the
  host (`https://www.paypal.com@evil.example/`) is left out: the host you read is the
  one that opens.
- When the link's text looks like an address on another domain than the one the link
  opens (the text says `www.paypal.com`, the link goes to `evil.example`), the box turns
  red and says so. It's one of the checks of Maze's phishing detection.

A click (also cmd-click or the middle button) opens web links in your browser, with
spaces and other characters a browser doesn't take encoded. A `mailto:` link starts a
new message to its address (with its subject and text, if it has them). Maze opens no
other kinds of links (`javascript:`, `file:`, `data:`, apps' own schemes) and says so. A
deceptive link asks first: "This link goes to evil.example, not paypal.com. Open
anyway?". A link that isn't a valid address shows a message with a **Copy** button
instead. Links in plain-text mail (`https://…`, `www.…`, `mailto:…`) are underlined and
work the same way.

### Searching

- **Quick Filter** (`cmd-f`, or Thunderbird's `cmd-shift-k`) narrows the folder you
  are looking at as you type, and its buttons show only unread, starred or tagged mail
  or mail with attachments. The list stays the folder; clear the filter and everything
  is back.
- **Search all messages** (`cmd-shift-f` as in Zed, or `cmd-/`) searches every account
  and folder, message text included, and shows the results as a list of their own.

Both understand free text plus Gmail-style operators, for example
`from:anna has:attachment after:2026-09-01`, `is:unread tag:todo`,
`subject:"offsite" -from:noreply`. The same queries work with `mazemail search`.

### Syncing

Maze syncs your accounts in the background. To sync all of them right away, click the
sync button in the title bar, next to **+** (New Message), press `cmd-shift-t`
(Thunderbird's "Get All New Messages"; `F5` works too), or run "Mail: Sync Now" from the
command palette. The icon turns while a sync runs. When sync has a problem (offline, the
account needs you to sign in again, an error), the button shows a warning dot and its
tooltip says why, like the status bar at the bottom.

To see exactly what is syncing, click the sync status at the bottom left of the status
bar ("Up to date", "Syncing…", "Offline"). **Sync activity** opens above it, with one
section per account:

- the state: connecting, syncing, up to date, offline, sign-in required or an error,
  with the reason. For offline accounts and errors, **Retry now** syncs that account
  right away;
- the folders being synced and what each one is doing, with progress where Maze knows
  it ("INBOX — fetching headers 1,200 / 5,400", "All Mail — re-reading labels"), then the
  folders waiting their turn;
- when the account last finished syncing ("Last synced 2 min ago", counted since Maze
  started);
- the work that waits: changes you made (moves, flags, uploads) that haven't reached the
  server yet, with the age of the oldest and those that failed too often, messages in
  the outbox, and messages being downloaded for offline reading.

It updates live while it is open; click outside it or press `escape` to close it.

### Notifications

When new mail arrives in an Inbox or in one of your [favorite folders](#the-folder-pane)
(except Sent), Maze shows a system notification with the sender and
the subject (with several accounts, also the account it arrived in). Click it and Maze
comes to the front with the message open in a tab. When more than three messages
arrive at once, one notification says how many and who sent them ("5 new messages,
From Bob, Carol and Dave"); a click brings Maze to the front.

- **macOS** uses Notification Center. Maze asks for permission before its first
  notification; change your answer, the banner style and sounds in System Settings →
  Notifications → Maze. Banners show while Maze is in front, too. Notifications need
  `Maze.app`, so a development build started with `cargo run` doesn't show any.
- **Linux** uses your desktop's notification service (GNOME, KDE, mako, dunst and
  others), with Maze's icon.

Only mail that just arrived counts: unread, in an Inbox or a favorite other than Sent,
from someone else, and
received by your server since Maze started syncing that account. The first sync of a
new account, mail that arrived while Maze wasn't running and mail moved back into the
Inbox stay quiet. Mail that arrives while your computer sleeps shows up once it wakes.

The `notifications` settings turn them off or show less:

```jsonc
"notifications": {
  "enabled": true,
  // "sender_and_subject", "sender_only" or "none" (only that new mail arrived)
  "content": "sender_and_subject"
}
```

### Disk space

The bottom right of the status bar shows how much disk space Maze uses, like "2.4 GB".
Hover over it for the breakdown: the encrypted mail store (database and messages), the
search index, caches, database backups taken before updates, logs and downloaded
updates, and where the data directory is. Maze measures it in the background: when it
starts, every few minutes, and soon after mail arrives, is imported or deleted (at most
every 30 seconds). Nothing shows until the first measurement is done.

## Writing mail

Press `cmd-n`, click **+** in the title bar, or choose **Message → New Message**. The
draft opens in a tab of its own (see [Tabs and splits](#tabs-and-splits)), named with
two random words such as `brave-otter`; split the window to keep the Inbox next to it:

- **From** picks the identity you send as. Replies pick it for you: the identity whose
  address the message was sent to (To, Cc or `Delivered-To`), else the account's default.
- **To**, **Cc** and **Bcc** (click **Cc Bcc** or press `cmd-shift-c`) show each
  recipient as a chip with their initials and name (hover for the address). See
  [Recipients](#recipients) below.
- The body is **Markdown**: `**bold**`, `*italic*`, lists, `> quotes`, links. Maze sends
  your text as the plain-text part and renders it as the HTML part, so recipients see
  normal formatted mail. Single line breaks are kept. A new line starts at the
  indentation of the one before, so nested lists stay nested; `tab` and `shift-tab`
  indent and outdent.
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

Reply (`cmd-r`, or `r` in the list and the message), reply all (`cmd-shift-r`, or
`shift-r`) and forward (`cmd-l`) open the same kind of tab with the recipients, the
`Re:`/`Fwd:` subject and the quoted message filled in (HTML mail is quoted as
Markdown).

Messages are sent right away. When the server can't be reached, the message waits in
the outbox and goes out by itself when it can; a message the server refuses (a wrong
password, a rejected recipient, Microsoft 365's *SendAsDenied* for an address you
aren't allowed to send from) keeps the draft open with the explanation. Maze records
every message the server accepted, so even a crash in the middle of sending never
sends it twice. The copy in your Sent folder is described under
[SMTP servers](#smtp-servers).

Drafts are saved on your computer (encrypted, like your mail) a moment after you stop
typing. Not there yet: drafts in the server's Drafts folder, forwarding the original's
attachments, signatures switching with the identity, send later and undo send.

### Recipients

Type an address into To, Cc or Bcc and end it with a comma, a semicolon, `enter` or
`tab` (or just click elsewhere): it becomes a chip. Pasting a list such as
`Anna <anna@example.com>, "Doe, John" <john@example.com>; bob@example.org` makes one
chip per address. Text that isn't an address becomes a red chip, and Maze won't send or
autosave until you fix or remove it; double-click any chip to edit it as text.

While you type, Maze suggests addresses after a moment: entries of your address books
first (the ones imported from Thunderbird), then addresses you have written to. Every
address you send a message to (To, Cc and Bcc, your own addresses left out) is
remembered on your computer, in the encrypted database, with how often you wrote to
it, so the people you write to most come first. `up` and `down` move through the
suggestions, `enter` or `tab` takes one, `escape` closes the list; a click works too.
Syncing your Google and Microsoft contacts comes later
([ADR-024](adr/0024-server-owned-contacts.md)).

Click a chip to select it, then `backspace` or `delete` removes it; `backspace` in the
empty field selects the last chip, and `left` / `right` move between chips. **Drag a
chip** to move a recipient between To, Cc and Bcc (or to another place in the same
field). When Cc and Bcc are hidden they appear while you drag, and stay open when you
drop a chip there.

### Spell checking

Maze checks the body as you type and underlines misspelled words with a red wavy line
a moment after you stop typing. Dictionaries for German (Germany, Austria and
Switzerland) and English (US and UK) are built in and work offline; nothing you write
leaves your computer. (The dictionaries come from
[wooorm/dictionaries](https://github.com/wooorm/dictionaries): SCOWL for English,
igerman98 for German; licenses in
[`third_party/dictionaries`](../third_party/dictionaries/README.md).)

- **Right-click** an underlined word for up to five suggestions; choosing one replaces
  the word (`cmd-z` undoes it). **Ignore Spelling** accepts the word in this draft
  only. **Add to Dictionary** accepts it in every draft from now on.
- **Your dictionary** (the words you added) is kept in Maze's encrypted database,
  because they are often names, and applies to every language. There's no list to edit
  yet: a word added by mistake stays until a later version can remove it.
- **Mixed languages:** each paragraph is checked in its own language, detected from
  its words, and also accepts the words of your other spelling languages. With German
  and English on, "tomorrow" in a German paragraph and "Termin" in an English one are
  fine. Short paragraphs (a greeting, a sign-off) accept any of your languages.
- **Not checked:** quoted lines (`>`), code (`` `inline` `` and fenced blocks), links,
  web and email addresses, words with digits and words in ALL CAPS. The subject line
  isn't checked yet.
- **The status item** at the bottom of the draft, next to **Send**, shows the languages
  and how many words are underlined, for example "✓ Spelling de-AT · en-US" or
  "2 spelling issues · de-AT · en-US". Click it to turn languages on or off for this
  draft only.

The languages come from the `spellcheck` setting. By default (`[]`) Maze uses your
system's preferred languages that have a dictionary (on macOS the list in **System
Settings → General → Language & Region**; on Linux `LANGUAGE`, `LC_ALL`,
`LC_MESSAGES` and `LANG`), and English (US) when none has. Name them yourself, best
first (a German paragraph is checked as the first German language you list), or turn
spell checking off:

```jsonc
{
  "spellcheck": ["de-AT", "en-US"]   // or [] for the system's languages, or false
}
```

The tags are `de-DE`, `de-AT`, `de-CH`, `en-US` and `en-GB` (`de_AT` and `de` work
too); Maze lists any other tag as a warning under the settings editor and ignores it.
Changes apply to open drafts right away.

## Agents

The agent column on the right (`cmd-?` shows and hides it) runs coding agents over the
Agent Client Protocol: Claude Code, Codex, Gemini CLI, Cursor, pi or your own. AI is
opt-in: it's off for every account until you share one with agents
(`"agent": { "accounts": { "Work": "any" } }`).

- **Install and sign in** from the column. Maze installs each agent (and the Node
  runtime npm agents need) itself and keeps installed agents up to date in the
  background; signing in, or pasting an API key, happens in a card in the column.
- **Conversations** start with the selected thread attached as context. The bar under
  the prompt picks the agent's mode, model and thinking effort, where the agent offers
  them.
- **Sandboxed:** every agent runs in an operating system sandbox (Seatbelt on macOS,
  bubblewrap on Linux). It can't read Maze's store or your home folder and reaches mail
  only through the `mazemail` CLI, with a token limited to the accounts you shared.
- **Proposals:** by default an agent's archive, move, tag, flag and mark-read requests
  become proposals at the top of the column. Accept or reject each item (`y` / `n`) or
  all of them (`cmd-shift-enter` / `cmd-shift-backspace`).

Everything else (permission prompts, keys, the sandbox policy, custom agents) is in
[agents.md](agents.md).

## Settings

Press `cmd-,` (or choose **Maze → Settings…**, or "Maze: Open Settings" in the command
palette) to edit `settings.json` in a tab. Maze creates the file if it doesn't exist
yet. It's a plain JSON editor (comments and trailing commas are allowed):

- `cmd-s` saves and applies the settings at once. The tab shows a dot while it has
  unsaved changes, and closing it asks whether to save them.
- Problems are listed below the editor with their line: syntax errors, keys Maze
  doesn't know (ignored), values of the wrong type (that key uses its default) and
  values Maze can't use, such as a spell checking language without a dictionary
  (ignored). A
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

### Font size

`"font_size"` is the base size of Maze's text in pixels, 14 by default:

```jsonc
"font_size": 16
```

Everything sized from it grows and shrinks with it: text, spacing, message list rows
(cards and table), reply indents, tab bars, avatars, the command palette and Quick Move.
Values from 10 to 32 are applied; smaller or larger ones are brought into that range.
The change applies as soon as you save. Mail shown in the original HTML view keeps the
sizes its sender chose.

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

## Keyboard shortcuts

Maze uses Thunderbird's shortcuts and fills the gaps with Zed's. The table writes the
main modifier as `cmd`: that is Command on macOS and Ctrl on Linux. To use Ctrl on a
Mac or the Super key on Linux, set it in `settings.json` and restart Maze:

```jsonc
"keymap": {
  // "platform" (Command on macOS, Ctrl on Linux), "ctrl" or "super"
  "primary_modifier": "ctrl"
}
```

With Ctrl as the main modifier, shortcuts that already use Ctrl next to `cmd` (splitting
with `cmd-ctrl-<arrow>`) use Ctrl and Super. Tooltips and hints in Maze show the keys
you chose. These are bound today:

| Action                                      | Key                                                  |
| ------------------------------------------- | ---------------------------------------------------- |
| **Window**                                  |                                                      |
| Command palette                             | `cmd-shift-p`                                        |
| Settings (`settings.json` in a tab)         | `cmd-,`                                              |
| Go to Folder                                | `cmd-k`                                              |
| Search all messages                         | `cmd-shift-f`, `cmd-/`                               |
| Quick Filter                                | `cmd-f`, `cmd-shift-k`                               |
| Toggle folder pane                          | `cmd-b` (or the title bar's left button)             |
| Toggle message pane                         | `F8`                                                 |
| Toggle agent column                         | `cmd-?`                                              |
| Table view / threads on or off              | `cmd-alt-t` / `cmd-alt-g`                            |
| Reader / original HTML                      | `cmd-alt-o`                                          |
| Sync all accounts now                       | `cmd-shift-t`, `F5` (or the title bar's sync button) |
| Hide Maze / hide others / minimize          | `cmd-h` / `cmd-alt-h` / `cmd-m`                      |
| Quit                                        | `cmd-q`                                              |
| **Message list and message**                |                                                      |
| Next / previous message                     | `f` / `b`, `down` / `up`                             |
| Next / previous unread                      | `n` / `p`                                            |
| First / last message, page down / up        | `home` / `end`, `pagedown` / `pageup`                |
| Select several rows / all rows              | `shift-down` / `shift-up`, `cmd-a`                   |
| Open / close a thread                       | `right` / `left`                                     |
| Open / close all threads                    | `*` / `\`                                            |
| Select the whole thread                     | `cmd-shift-a`                                        |
| Open the message in a tab                   | `enter`, `cmd-o`, double-click                       |
| Toggle read                                 | `m`                                                  |
| Star                                        | `s`                                                  |
| Archive                                     | `a`                                                  |
| Report junk (mark and move to Junk)         | `j`                                                  |
| Move to a folder (Quick Move)               | `shift-m`                                            |
| Delete (move to Trash)                      | `backspace`, `delete`                                |
| Undo the last archive, move, delete or flag | `cmd-z`                                              |
| **Writing**                                 |                                                      |
| New message                                 | `cmd-n`                                              |
| Reply / reply all / forward                 | `cmd-r` / `cmd-shift-r` / `cmd-l`                    |
| Reply / reply all (list and message)        | `r` / `shift-r`                                      |
| Send                                        | `cmd-enter`                                          |
| Save draft / attach / Cc and Bcc            | `cmd-s` / `cmd-shift-a` / `cmd-shift-c`              |
| Recipient: make a chip / take a suggestion   | `,` `;` `enter` `tab` / `enter`, `tab`               |
| Recipient: select the last chip / remove it  | `backspace` / `backspace`, `delete`                  |
| Recipient: move between chips                | `left` / `right`                                     |
| Close the draft (asks to keep changes)      | `escape`                                             |
| **Tabs and panes**                          |                                                      |
| Close the tab                               | `cmd-w`                                              |
| Next / previous tab                         | `ctrl-tab` / `ctrl-shift-tab`, `cmd-}` / `cmd-{`     |
| Tab 1 … 8 / last tab                        | `cmd-1` … `cmd-8` / `cmd-9`                          |
| Split right                                 | `cmd-\`                                              |
| Split left / right / up / down              | `cmd-ctrl-left` / `right` / `up` / `down`            |
| Focus the pane left / right / above / below | `cmd-alt-left` / `right` / `up` / `down`             |
| **Settings tab**                            |                                                      |
| Save and apply                              | `cmd-s`                                              |
| **Agent column**                            |                                                      |
| New agent conversation                      | `cmd-n`                                              |
| Cycle mode / model / thinking (prompt)      | `shift-tab` / `alt-tab` / `ctrl-'`                   |
| Accept / reject all proposals               | `cmd-shift-enter` / `cmd-shift-backspace`            |
| Accept / reject a proposal item, move       | `y` / `n`, `down` / `up` (review focused)            |

Single-letter shortcuts work when the message list or a message has focus, never inside
a text field; they act on the tab that has the focus.

Zed splits and moves between panes with `cmd-k` followed by an arrow key. In Maze
`cmd-k` is Go to Folder, and a two-key shortcut starting with `cmd-k` would make it
wait a second for the second key, so Maze uses `cmd-ctrl-<arrow>` and
`cmd-alt-<arrow>` instead. Maze doesn't read a keymap of your own (`keymap.json`) yet.
The rest of Thunderbird's keymap (tags `1`–`9`, junk, history) follows as the features
arrive.

## Getting help

- `mazemail --help` and `mazemail help <command>` for the CLI
- [GitHub issues](https://github.com/pmig/maze/issues) for bugs and questions
- Security problems privately to philip@pmig.at, see [SECURITY.md](../SECURITY.md)

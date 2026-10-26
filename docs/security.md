# Security in Maze

Maze keeps a full copy of your mail on your computer, so it encrypts it. This page
explains what is protected and from whom, where the key lives, what stays in plaintext
on purpose, what to do when the key is lost, and how the CLI and agents get access.
The design is in [ADR-012](adr/0012-encryption-at-rest.md),
[ADR-017](adr/0017-account-secrets-in-the-vault.md),
[ADR-006](adr/0006-agent-interface.md) and [ADR-013](adr/0013-agent-sandbox.md).
To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## What encryption at rest protects against

- **Copies of your data directory**: Time Machine and other backups, cloud sync, a
  stolen disk without FileVault. Without the key, they're unreadable.
- **Programs running as you that don't go through Maze**: scripts, agents started
  outside Maze, malware without keychain access. They can read the files, but not the
  mail in them, and the running app only answers requests with a token you approved.

It does **not** protect against root, a process that can debug Maze (signed release
builds use the hardened runtime, which blocks this), someone you allow into the
keychain item, and plaintext Maze hands out because you asked for it (below).

**Linux is weaker than macOS.** Any process running as you can read an unlocked
Secret Service collection, and with it the key. On Linux, encryption protects copies
and backups; it doesn't keep out local programs running as you.

## What is encrypted

Everything with mail content lives in `mail/` inside the data directory
(`~/Library/Application Support/mazemail` on macOS, `$XDG_DATA_HOME/mazemail` on
Linux), and all of it is encrypted:

| Data                                                 | Encryption                                                                   |
| ---------------------------------------------------- | ---------------------------------------------------------------------------- |
| Database `mazemail.db` (messages, folders, accounts, identities, account passwords and OAuth refresh tokens, API token hashes, audit log) | SQLCipher 4: AES-256 with an HMAC-SHA512 per page |
| Raw messages and attachments (`blobs/`), caches (`cache/`) | XChaCha20-Poly1305 in 64 KiB chunks, bound to the blob's id, after compression |
| Search index (`index/`)                              | Tantivy segments in 16 KiB XChaCha20-Poly1305 blocks                         |
| Pre-migration database backups (`backups/`)          | SQLCipher, same key                                                          |

Each part uses its own subkey, derived with HKDF-SHA256 from one 256-bit **master
key**. Encrypted files carry the id of the key that sealed them, so the key can be
rotated later.

Account passwords, SMTP passwords and OAuth refresh tokens are in the encrypted
database too, under the same master key as the mail they unlock
([ADR-017](adr/0017-account-secrets-in-the-vault.md)). Whoever can read the master key
can read all your mail anyway, so separate keychain items would add a system prompt
per account and secret but little protection. The master key is Maze's **only**
keychain item: a launch reads the keychain once. Secrets are wiped from memory after
use and never logged.

API keys and tokens you paste into the agent column's sign-in card are stored the same
way (table `agent_secret`) and handed only to that agent, in its environment. Keys
written into `agent.env` in `settings.json` are plain text; the settings file says so.

Earlier versions of Maze kept each of these secrets as its own keychain item (service
`mazemail`, account `<account-uuid>/<kind>`, labelled "Maze account password", "Maze
sign-in token" or "Maze SMTP password"). The first start of a newer Maze moves them
into the encrypted database and deletes the items, so macOS may ask one last time for
each of them (to read it, and possibly to delete it). If you deny a read, Maze asks
again at the next start; if it can't delete an item after copying it, it logs a
warning and leaves it, and you can delete it in Keychain Access (or Seahorse on
Linux).

## Where the key lives

- The master key is 32 random bytes, created on the first launch, read once on a
  background thread at startup and wiped from memory when Maze quits. It's the only
  thing Maze keeps in the keychain.
- **macOS**: a keychain item named "Maze storage key" (service
  `dev.mazemail.maze.storage`) in your login keychain. Signed release builds are meant
  to move it into the data protection keychain, restricted to apps signed by the Maze
  team; that needs the notarized build with its provisioning profile.
- **Linux**: an item in the Secret Service (GNOME Keyring, KWallet).
- **Development builds** use their own item (`dev.mazemail.maze-dev.storage`) and their
  own data directory, so they never touch your real store.
- `MAZEMAIL_INSECURE_DEV_KEYSTORE=1` keeps the key in a plain file next to the data,
  which protects nothing. It exists for demos and automated tests, is never the
  default, and Maze logs a warning on every start with it.

## What stays in plaintext, on purpose

- **CLI output**: whatever a token you approved asks for.
- **Exports**: the mbox or Maildir files `mazemail export` writes. Every export asks
  for your approval first.
- **The Thunderbird profile you imported from**: Maze doesn't change or delete it, and
  it still holds all your mail in plaintext.
- **Settings, keymaps and themes** in `~/.config/mazemail/`. They hold no mail content
  and no addresses; accounts and identities are in the encrypted database.
- **Logs**: Maze never logs message content, subjects or addresses.

- **Agent session folders** (`sessions/<id>/` in the data directory): the thread you
  attached to a conversation, as Markdown, because the agent has to read it. Only what
  that session was given; deleted when the session ends, leftovers at the next start.
- **Agent homes** (`agent-home/<id>/`): each agent's own configuration and login, as
  the agent writes them (Claude Code's subscription login is
  `.claude/.credentials.json` there, because the sandbox keeps it out of your keychain).
  Terminal login scripts (`agents/login/`) hold no keys.

As the features arrive, these join the list: attachments you open in another app (a
private temporary folder, deleted on quit or after 24 hours), notification text (macOS
keeps delivered notifications in its own database; the `notifications.content` setting
controls whether they include the subject) and pages you open in a browser. The
original HTML view writes nothing (see [HTML mail](#html-mail)). Maze
doesn't donate mail to Spotlight.

## HTML mail

Every HTML message goes through the same pipeline before anything shows it: Maze
parses it and runs it through a strict sanitizer (an allowlist built on `ammonia`) that
removes scripts, frames, objects and embeds, forms, `meta refresh`, `base`, every `on…`
event attribute and `javascript:` URLs, and drops known tracking pixels.

**Reader mode** draws the sanitized text natively and loads nothing remote.

**Original mode** (ADR-020) hands the sanitized page to an embedded Servo engine,
rendered offscreen into an image inside the Maze window. The engine is locked down in
layers, each of which holds on its own:

- **No scripts.** The page is served with `Content-Security-Policy: sandbox;
  default-src 'none'; img-src <message origin> data:; style-src 'unsafe-inline';
  base-uri 'none'; form-action 'none'`. `default-src 'none'` forbids every script, and
  the `sandbox` directive (no `allow-scripts`) turns scripting off for the page
  altogether, so even an inline handler the sanitizer missed doesn't run. Maze's
  tests load a page with a script and an `onload` handler into the real engine and
  check that neither ran. Servo is also built without its JIT (so Maze needs no JIT
  entitlement) and with its faster JavaScript tiers and WebAssembly switched off.
  SpiderMonkey's basic interpreter can't be left out of Servo, which is why the
  sandbox and the CSP, not the build, are what keep scripts from running.
- **Nothing reaches the network or the disk from the engine.** The page lives at
  `https://message.mazemail.localhost/m<n>/` and every load the engine makes (the
  page, its images, stylesheets, frames, anything) goes to Maze first. Maze answers
  the page and the message's inline (`cid:`) images from memory and refuses
  everything else; loads of a message the view has moved on from are refused too.
  Remote images are fetched by Maze, never by the engine, and only when allowed.
- **No storage.** The engine runs with temporary storage only (no profile
  directory, no disk cache, no cookies kept), no devtools, no service or shared
  workers, no IndexedDB and no web APIs (WebGL, WebGPU, WebXR, Bluetooth,
  notifications, geolocation, clipboard reading). The message is never written to a
  file: no temporary HTML, no cache.
- **Links leave Maze.** A click on a link doesn't navigate the page: the engine
  reports it and Maze applies the reader's link policy (normalized targets, web links
  to your browser, `mailto:` to a new draft, every other scheme refused, a
  confirmation first for deceptive links whose text shows another domain). Hovering a
  link shows its real target, as in the reader.
- **It's an image in the window.** The engine never gets a native view: it paints
  into an offscreen buffer that Maze draws like any picture, so it can't cover dialogs
  or menus, and keyboard focus and shortcuts stay with Maze.

**Remote images** follow `reader.remote_content` (per account): `"ask"` blocks them
until you choose **Load images** (this message, once) or **Always for this sender**
(kept in the encrypted database, table `remote_content_sender`); `"never"` blocks them
even for remembered senders; `"always"` loads them. Maze fetches them with its own HTTP
client: only URLs the sanitized message references, only images, at most 5 MB each,
8 seconds at most, no cookies, no `Referer` and a generic `User-Agent`. What a sender
can still learn when you load images: that the message was opened, when, and from
which IP address (and roughly where, from that address); which images you loaded if
their URLs are unique to you. Tracking pixels Maze recognizes are dropped even then.

## Lost key and recovery

Maze keeps all mail in server accounts ([ADR-019](adr/0019-imap-only.md)), so `mail/` is
a copy of what's on your servers and a lost key means syncing again. What only exists
locally would be lost: uploads from an import that haven't reached the server yet, tags
and saved searches that aren't on a server, API tokens, the audit log, and Local Folders
left over from earlier versions of Maze (export them, see
[Getting started](getting-started.md#local-folders-from-earlier-versions)).

If Maze finds encrypted mail but no key (a new Mac, a reset keychain, a restored
backup), it opens a dialog that can't be dismissed, with two ways out:

- **Enter recovery key**: the recovery key is the master key written as 64 characters
  in 16 groups of four (`7K2Q-…-M4XA`). Case, spaces and hyphens don't matter, and the
  last group is a checksum, so a typo is caught. The key is checked against your data
  before Maze stores it; a wrong key keeps the dialog open.
- **Start fresh and re-sync**: after a confirmation, Maze moves the unreadable data
  aside to `mail-unreadable-<time>/` in the data directory (it never deletes it) and
  creates a new, empty store.

Maze can't show you the recovery key yet (it's planned under Settings → Privacy, behind
Touch ID or your password). Until then the keychain item is the only copy of the key,
so keep the originals of anything you import until the upload has reached your server.

## Local API tokens

The CLI never opens the store; it asks the running app over a Unix socket in the data
directory (mode 0600, outside `mail/`). Details are in [cli.md](cli.md); the security
rules are:

- **Every request that touches mail needs a token.** There is no tokenless access.
  Without one, only `help`, `status` and `login` work.
- **You approve every token** in a dialog that shows the requesting program (path and
  parent process, from the socket's peer credentials), the profile, the scope and the
  lifetime. Tokens can't mint tokens: a program that already holds one still goes
  through the dialog.
- Tokens are 256-bit random secrets with the prefix `mazemail_s_`, stored only as a
  BLAKE3 hash, with a profile (read-only, triage or writer), a scope (accounts,
  folders), an expiry (at most 30 days) and a label. `mazemail token revoke` revokes
  one.
- The server rejects connections from other user ids, and records every request (the
  program's pid and path, the outcome, the affected ids, never content) in the
  encrypted audit log.
- Commands that leave the store or change a lot (`import`, `export`, rules, sending)
  ask again for each request, and permanent delete isn't available to tokens at all.
- Accounts whose AI policy excludes agents never appear in any token's scope.

## Agents

Agents you start yourself, outside Maze, are not sandboxed by Maze. Encryption keeps
them out of the store, and they see only what the token you gave them allows. Prefer
`read-only` or `triage` tokens with a short lifetime and a label.

Agents that Maze starts itself (the agent column) run in an operating system sandbox:
Seatbelt on macOS, bubblewrap on Linux (the seccomp filter against new Unix sockets is
not implemented yet). The policy hides your home directory except the agent's own home
and its session folder, never lets an agent read `mail/`, other sessions, `~/.ssh` or
the Thunderbird profile, lets it write only to its session and its own home, and allows
no Unix socket except Maze's. Agents start with an empty environment plus what Maze
sets. Each session gets a token scoped to the accounts you share with agents, revoked
when it ends. Network access stays on, because agents have to reach their model
provider. AI is off for every account unless you turn it on. Details in
[agents.md](agents.md).

## Updates

The built-in updater only installs releases whose update manifest is signed with Maze's
Ed25519 key, checks the SHA-256 of every download against it, and on macOS requires the
new bundle to be signed by the same Apple team as the running app. It sends nothing but
a plain request with `User-Agent: mazemail/<version>`.

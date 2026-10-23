# Mail access with the `mazemail` CLI

<!-- Generated from the Maze command registry (`local_api`). Do not edit. -->

`mazemail` talks to the running Maze app over a local socket; it never opens the mail store itself. Every command prints a readable table on a terminal and stable JSON with `--json`. Ids are short and stable: `m_…` message, `t_…` thread, `d_…` draft, `f_…` folder, `a_…` account, `tk_…` token.

## Signing in

- Commands that touch mail need a session token in `MAZEMAIL_SESSION`. Agents started by Maze already have one.
- Otherwise run `eval "$(mazemail login --profile read-only --ttl 8h)"`. Maze shows which program is asking and waits for approval ("waiting for approval in Maze…").
- Without a token only `mazemail help`, `mazemail status` and `mazemail login` work (exit code 6).
- Your token decides what you can do (see the table below) and which accounts and folders you can see. Accounts whose AI policy excludes agents are never visible.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| 0 | ok |
| 1 | other failure |
| 2 | usage error |
| 3 | permission denied |
| 4 | approval denied |
| 5 | Maze is not running |
| 6 | not signed in (no or expired token) |

## What each profile may do

`allow` runs directly, `propose` becomes a proposal you review in Maze, `ask` waits for approval in Maze, `deny` fails with exit code 3.

| Command | read-only | triage | writer |
| ------- | --- | --- | --- |
| `status` | allow (no token needed) | allow (no token needed) | allow (no token needed) |
| `help` | allow (no token needed) | allow (no token needed) | allow (no token needed) |
| `login` | ask (no token needed) | ask (no token needed) | ask (no token needed) |
| `token create` | ask (no token needed) | ask (no token needed) | ask (no token needed) |
| `token list` | allow | allow | allow |
| `token revoke` | deny | ask | ask |
| `search` | allow | allow | allow |
| `show` | allow | allow | allow |
| `folders` | allow | allow | allow |
| `tags` | allow | allow | allow |
| `contacts` | allow | allow | allow |
| `identities` | allow | allow | allow |
| `attachment save` | allow | allow | allow |
| `draft new` | deny | allow | allow |
| `draft edit` | deny | allow | allow |
| `archive` | deny | propose | allow |
| `move` | deny | propose | allow |
| `tag` | deny | propose | allow |
| `flag` | deny | propose | allow |
| `read` | deny | propose | allow |
| `propose` | deny | allow | allow |
| `rule add` | deny | ask | ask |
| `send` | deny | deny (ask with `--allow-send`) | deny (ask with `--allow-send`) |
| `export` | deny | ask | ask |
| `import` | deny | ask | ask |
| `account list` | allow | allow | allow |
| `account add` | deny | deny | ask |
| `account remove` | deny | deny | ask |
| `sync` | allow | allow | allow |

Permanent delete is not available to tokens. `send` is disabled unless the token was approved with `--allow-send`, and then every send asks.

## Commands

### `mazemail status`

Show whether Maze is running and whether you are signed in.

Shows Maze's version and protocol, and the current token's profile, scope and expiry. Works without a token.

Permission: `read` (works without a token)

```sh
mazemail status
```

Output (`--json`):

```ts
{
  app: any,  // Details from the app (accounts, sync state; only when signed in).
  protocol: integer,  // Protocol major version.
  running: boolean,  // Always `true` (otherwise the CLI exits with code 5).
  signed_in: boolean,  // Whether the request carried a valid token.
  token?: {
    allow_send: boolean,  // Whether `send` is enabled (always asks).
    created_at: string,  // Creation time (RFC 3339).
    created_by: {
      kind: "user",
    }
    | {
      kind: "session",
      session: string,  // The agent session's id.
    },  // Who created it.
    expires_at?: string | null,  // Expiry (RFC 3339), `null` = never.
    id: string,  // Public id (`tk_…`).
    label?: string | null,  // Label.
    profile: "read-only" | "triage" | "writer",  // Profile.
    revoked: boolean,  // Whether it was revoked.
    scope: {
      accounts?: string[] | null,  // Account ids (`a_…`), or `null` for all.
      folders?: string[] | null,  // Folder ids (`f_…`), or `null` for all.
    },  // Accounts and folders (`null` = all).
  }
  | null,  // The token, when signed in.
  token_error?: string | null,  // Why a presented token was not accepted ("the session token has expired").
  version: string,  // Maze's version.
}
```

Examples:

```sh
# Check the session
mazemail status --json | jq .signed_in
```

### `mazemail help`

List commands and what your token may do with them.

Without a topic, lists every command with its permission and what happens with your token (allow, propose, ask or deny). `mazemail help <command>` shows a command's help, `mazemail help agents` prints this AGENTS.md. Works without a token.

Permission: `read` (works without a token)

```sh
mazemail help [<TOPIC>...]
```

| Argument | Description |
| -------- | ----------- |
| `<TOPIC>...` | A command (`draft new`) or `agents` for the AGENTS.md text. |

Output (`--json`):

```ts
{
  commands: [
    {
      access?: "allow" | "propose" | "ask" | "deny" | null,  // What happens with the caller's token (`null` without a token).
      command: string,  // As typed after `mazemail` (`draft new`).
      permission: "read" | "draft" | "propose" | "ask" | "disabled" | "admin",  // Permission class.
      requires_token: boolean,  // Whether the command needs a token.
      summary: string,  // One-line summary.
    }
  ],  // Every command.
}
```

Examples:

```sh
# Help for one command
mazemail help draft new
# Write the agent guide
mazemail help agents > AGENTS.md
```

### `mazemail login`

Ask Maze for a session token.

Maze shows which program is asking, the profile, scope and lifetime, and waits for your approval. Approved, it prints `export MAZEMAIL_SESSION=…` for the current shell. A program that already holds a token still needs approval, and the new token gets exactly what you approve. Accounts whose AI policy excludes agents are never in a token's scope.

Permission: `ask` (works without a token)

```sh
mazemail login [OPTIONS]
```

| Argument | Description |
| -------- | ----------- |
| `--profile <PROFILE>` | Profile of the new token. One of `read-only`, `triage`, `writer`. Default: `read-only`. |
| `--ttl <TTL>` | How long the token is valid: 30m, 8h, 7d, 1h30m (at most 30d). Default: `8h`. |
| `--label <LABEL>` | A label shown in Settings and the audit log. |
| `--account <ACCOUNT>...` | Limit the token to this account (repeatable; default: every account whose AI policy allows agents). |
| `--folder <FOLDER>...` | Limit the token to this folder (repeatable; default: all folders). |
| `--allow-send` | Enable `mazemail send` for the token (every send still asks). |

Output (`--json`):

```ts
{
  allow_send: boolean,  // Whether `send` is enabled (always asks).
  expires_at?: string | null,  // Expiry (RFC 3339).
  export: string,  // `export MAZEMAIL_SESSION=…`, for `eval "$(mazemail login)"`.
  id: string,  // The token's public id.
  profile: "read-only" | "triage" | "writer",  // The approved profile.
  scope: {
    accounts?: string[] | null,  // Account ids (`a_…`), or `null` for all.
    folders?: string[] | null,  // Folder ids (`f_…`), or `null` for all.
  },  // The approved scope (`null` = all).
  token: string,  // The new token. Shown once; Maze keeps only a hash.
}
```

Examples:

```sh
# Sign in the current shell
eval "$(mazemail login --profile read-only --ttl 8h)"
# A triage token for one account
mazemail login --profile triage --account a_b --label claude
```

### `mazemail token create`

Ask Maze for a session token (same as `mazemail login`).

An alias of `mazemail login`.

Permission: `ask` (works without a token)

```sh
mazemail token create [OPTIONS]
```

| Argument | Description |
| -------- | ----------- |
| `--profile <PROFILE>` | Profile of the new token. One of `read-only`, `triage`, `writer`. Default: `read-only`. |
| `--ttl <TTL>` | How long the token is valid: 30m, 8h, 7d, 1h30m (at most 30d). Default: `8h`. |
| `--label <LABEL>` | A label shown in Settings and the audit log. |
| `--account <ACCOUNT>...` | Limit the token to this account (repeatable; default: every account whose AI policy allows agents). |
| `--folder <FOLDER>...` | Limit the token to this folder (repeatable; default: all folders). |
| `--allow-send` | Enable `mazemail send` for the token (every send still asks). |

Output (`--json`):

```ts
{
  allow_send: boolean,  // Whether `send` is enabled (always asks).
  expires_at?: string | null,  // Expiry (RFC 3339).
  export: string,  // `export MAZEMAIL_SESSION=…`, for `eval "$(mazemail login)"`.
  id: string,  // The token's public id.
  profile: "read-only" | "triage" | "writer",  // The approved profile.
  scope: {
    accounts?: string[] | null,  // Account ids (`a_…`), or `null` for all.
    folders?: string[] | null,  // Folder ids (`f_…`), or `null` for all.
  },  // The approved scope (`null` = all).
  token: string,  // The new token. Shown once; Maze keeps only a hash.
}
```

Examples:

```sh
# Print just the token
mazemail token create --profile read-only --ttl 1h --json | jq -r .token
```

### `mazemail token list`

List API tokens.

Lists every token with its profile, scope, expiry and creator. Secrets are never shown; Maze only keeps hashes.

Permission: `read`

```sh
mazemail token list
```

Output (`--json`):

```ts
[
  {
    allow_send: boolean,  // Whether `send` is enabled (always asks).
    created_at: string,  // Creation time (RFC 3339).
    created_by: {
      kind: "user",
    }
    | {
      kind: "session",
      session: string,  // The agent session's id.
    },  // Who created it.
    expires_at?: string | null,  // Expiry (RFC 3339), `null` = never.
    id: string,  // Public id (`tk_…`).
    label?: string | null,  // Label.
    profile: "read-only" | "triage" | "writer",  // Profile.
    revoked: boolean,  // Whether it was revoked.
    scope: {
      accounts?: string[] | null,  // Account ids (`a_…`), or `null` for all.
      folders?: string[] | null,  // Folder ids (`f_…`), or `null` for all.
    },  // Accounts and folders (`null` = all).
  }
]
```

Examples:

```sh
# Show tokens
mazemail token list
```

### `mazemail token revoke`

Revoke an API token.

Revoking your own token (the one in MAZEMAIL_SESSION) always works. Revoking another token asks for approval in Maze.

Permission: `ask`

```sh
mazemail token revoke <TOKEN_ID>
```

| Argument | Description |
| -------- | ----------- |
| `<TOKEN_ID>` | The token's id (`tk_…`, from `mazemail token list`). |

Output (`--json`):

```ts
{
  id: string,  // The token's id.
  revoked: boolean,  // `false` if it was already revoked.
}
```

Examples:

```sh
# Revoke a token
mazemail token revoke tk_q3x7k2mz4b5ca
```

### `mazemail search`

Search mail.

Searches every account and folder in your token's scope, newest first. The query language is Thunderbird's quick filter plus Gmail-style operators.

Permission: `read`

```sh
mazemail search [OPTIONS] <QUERY>
```

| Argument | Description |
| -------- | ----------- |
| `<QUERY>` | The query: words plus `from:`, `to:`, `subject:`, `has:attachment`, `is:unread`, `tag:`, `in:`, `list:`, `after:`, `before:`, `older_than:`. |
| `--limit <LIMIT>` | Maximum number of results. Default: `50`. |
| `--ids` | Print only message ids, one per line (for `$(…)`). |

Output (`--json`):

```ts
[
  {
    account: string,  // Account id.
    date: string,  // Date (RFC 3339).
    flagged: boolean,  // Flagged.
    folder?: string | null,  // Folder id.
    from: string,  // Sender ("Name <address>").
    has_attachments: boolean,  // Has attachments.
    id: string,  // Message id.
    snippet?: string | null,  // Start of the body.
    subject: string,  // Subject.
    tags: string[],  // Tag names.
    thread: string,  // Thread id.
    unread: boolean,  // Unread.
  }
]
```

Examples:

```sh
# Thread of the newest match
mazemail search 'from:anna has:attachment after:2026-09-01' --json | jq -r '.[0].thread'
# Ten unread messages
mazemail search 'is:unread' --limit 10
```

### `mazemail show`

Show a message or a whole thread.

Renders a message (`m_…`) or a thread (`t_…`, all messages oldest first) as Markdown, plain text or the raw message. Remote content is never loaded.

Permission: `read`

```sh
mazemail show [OPTIONS] <ID>
```

| Argument | Description |
| -------- | ----------- |
| `<ID>` | A message (`m_…`) or thread (`t_…`) id. |
| `--format <FORMAT>` | Output format. One of `md`, `text`, `raw`. Default: `md`. |

Output (`--json`):

```ts
{
  content: string,  // The message or whole thread rendered in `format`.
  format: "md" | "text" | "raw",  // Format of `content`.
  id: string,  // The requested message or thread id.
  messages: [
    {
      attachments: [
        {
          content_type: string,  // MIME type.
          filename: string,  // File name.
          id: string,  // Attachment id, for `mazemail attachment save`.
          size: integer,  // Size in bytes.
        }
      ],  // Attachments.
      date: string,  // Date (RFC 3339).
      from: string,  // Sender.
      id: string,  // Message id.
      subject: string,  // Subject.
      to: string[],  // Recipients.
    }
  ],  // The messages shown, oldest first.
  thread: string,  // Thread id.
}
```

Examples:

```sh
# A thread as Markdown
mazemail show t_19bd --format md
# Save the raw message
mazemail show m_8f3a2c --format raw > message.eml
```

### `mazemail folders`

List folders.

Lists the folders in your token's scope with unread and total counts.

Permission: `read`

```sh
mazemail folders [OPTIONS]
```

| Argument | Description |
| -------- | ----------- |
| `--account <ACCOUNT>` | Only this account's folders. |

Output (`--json`):

```ts
[
  {
    account: string,  // Account id.
    id: string,  // Folder id.
    name: string,  // Display name.
    path: string,  // Full path (`Archive/2026`).
    role?: string | null,  // Special-use role (`inbox`, `sent`, `drafts`, `trash`, `junk`, `archive`, …).
    total: integer,  // All messages.
    unread: integer,  // Unread messages.
  }
]
```

Examples:

```sh
# Find the archive folders
mazemail folders --json | jq -r '.[] | select(.role == "archive") | .id'
```

### `mazemail tags`

List tags.

Lists tags (IMAP keywords) with their colors.

Permission: `read`

```sh
mazemail tags
```

Output (`--json`):

```ts
[
  {
    color?: string | null,  // Color (`#rrggbb`).
    id: string,  // Tag id.
    name: string,  // Name.
  }
]
```

Examples:

```sh
# Show tags
mazemail tags
```

### `mazemail contacts`

Find contacts.

Finds people you have exchanged mail with by name or address.

Permission: `read`

```sh
mazemail contacts [OPTIONS] <QUERY>
```

| Argument | Description |
| -------- | ----------- |
| `<QUERY>` | Part of a name or address. |
| `--limit <LIMIT>` | Maximum number of results. Default: `20`. |

Output (`--json`):

```ts
[
  {
    email: string,  // Address.
    name?: string | null,  // Display name.
  }
]
```

Examples:

```sh
# Find Anna's address
mazemail contacts anna
```

### `mazemail identities`

List your sending identities.

Lists the identities (From addresses) of the accounts in scope. `draft new` picks one automatically when you don't pass --identity.

Permission: `read`

```sh
mazemail identities [OPTIONS]
```

| Argument | Description |
| -------- | ----------- |
| `--account <ACCOUNT>` | Only this account's identities. |

Output (`--json`):

```ts
[
  {
    account: string,  // Account id.
    default: boolean,  // The account's default identity.
    email: string,  // Address.
    id: string,  // Identity id.
    name: string,  // Display name.
  }
]
```

Examples:

```sh
# Show identities
mazemail identities
```

### `mazemail attachment save`

Save an attachment to a file.

Writes the attachment into --to (default: the current directory) under its sanitized file name, never overwriting an existing file.

Permission: `read`

```sh
mazemail attachment save [OPTIONS] <ID>
```

| Argument | Description |
| -------- | ----------- |
| `<ID>` | The attachment's id (from `mazemail show`). |
| `--to <DIR>` | Directory to save into (default: the current directory). |

Output (`--json`):

```ts
{
  content_type: string,  // MIME type.
  id: string,  // Attachment id.
  path: string,  // Where the file was written.
  size: integer,  // Size in bytes.
}
```

Examples:

```sh
# Save into the current directory
mazemail attachment save m_8f3a2c.2 --to .
```

### `mazemail draft new`

Write a new draft, reply or forward.

Creates a draft in Maze for you to review and send. Drafts are harmless: nothing is sent. The identity is picked automatically from the recipients and the replied-to message unless you pass --identity.

Permission: `draft`

```sh
mazemail draft new [OPTIONS]
```

| Argument | Description |
| -------- | ----------- |
| `--reply-to <ID>` | Reply to this message (`m_…`). |
| `--reply-all` | Reply to all recipients (with --reply-to). |
| `--forward <ID>` | Forward this message (`m_…`). |
| `--identity <IDENTITY>` | Identity to write from; picked automatically from the recipients and the replied-to message when omitted. |
| `--to <ADDRESS>...` | Recipient (repeatable). |
| `--cc <ADDRESS>...` | Cc recipient (repeatable). |
| `--bcc <ADDRESS>...` | Bcc recipient (repeatable). |
| `--subject <SUBJECT>` | Subject (default for replies: "Re: …"). |
| `--body-file <FILE>` | Read the body (Markdown) from a file, `-` for stdin. |
| `--body <BODY>` | The body (Markdown). |
| `--attach <FILE>...` | Attach a file (repeatable). |

Output (`--json`):

```ts
{
  id: string,  // Draft id.
  identity?: string | null,  // The identity it is written from.
  subject: string,  // Subject.
  to: string[],  // Recipients.
}
```

Examples:

```sh
# Reply; identity picked automatically
mazemail draft new --reply-to m_8f3a2c --body-file reply.md
# New message with the body from stdin
echo 'See you then.' | mazemail draft new --to anna@example.com --subject Friday --body-file -
```

### `mazemail draft edit`

Change a draft.

Replaces the given fields of a draft; everything else stays as it is.

Permission: `draft`

```sh
mazemail draft edit [OPTIONS] <ID>
```

| Argument | Description |
| -------- | ----------- |
| `<ID>` | The draft's id (`d_…`). |
| `--identity <IDENTITY>` | Identity to write from. |
| `--to <ADDRESS>...` | Replace the recipients (repeatable). |
| `--cc <ADDRESS>...` | Replace the Cc recipients (repeatable). |
| `--bcc <ADDRESS>...` | Replace the Bcc recipients (repeatable). |
| `--subject <SUBJECT>` | Replace the subject. |
| `--body-file <FILE>` | Replace the body with a file's content (Markdown), `-` for stdin. |
| `--body <BODY>` | Replace the body (Markdown). |
| `--attach <FILE>...` | Add an attachment (repeatable). |

Output (`--json`):

```ts
{
  id: string,  // Draft id.
  identity?: string | null,  // The identity it is written from.
  subject: string,  // Subject.
  to: string[],  // Recipients.
}
```

Examples:

```sh
# Replace subject and body
mazemail draft edit d_42 --subject 'Re: Friday' --body-file reply.md
```

### `mazemail archive`

Archive messages or threads.

Under the triage profile this becomes a proposal that you accept or reject in Maze's review UI; under the writer profile it runs directly.

Permission: `propose`

```sh
mazemail archive <ID>...
```

| Argument | Description |
| -------- | ----------- |
| `<ID>...` | Message (`m_…`) or thread (`t_…`) ids. |

Output (`--json`):

```ts
{
  affected: string[],  // Message ids affected.
  outcome: "applied" | "proposed",  // Applied directly, or a proposal.
  proposal?: string | null,  // The proposal's id, if proposed.
}
```

Examples:

```sh
# Archive old newsletters (a proposal under triage)
mazemail archive $(mazemail search 'list:news older_than:7d' --ids)
```

### `mazemail move`

Move messages or threads to a folder.

Under the triage profile this becomes a proposal that you accept or reject in Maze's review UI; under the writer profile it runs directly.

Permission: `propose`

```sh
mazemail move --to <FOLDER> <ID>...
```

| Argument | Description |
| -------- | ----------- |
| `--to <FOLDER>` | Target folder id (`f_…`, from `mazemail folders`). Required. |
| `<ID>...` | Message (`m_…`) or thread (`t_…`) ids. |

Output (`--json`):

```ts
{
  affected: string[],  // Message ids affected.
  outcome: "applied" | "proposed",  // Applied directly, or a proposal.
  proposal?: string | null,  // The proposal's id, if proposed.
}
```

Examples:

```sh
# Move to a folder
mazemail move --to f_k2 m_8f3a2c t_19bd
```

### `mazemail tag`

Add or remove tags.

Under the triage profile this becomes a proposal that you accept or reject in Maze's review UI; under the writer profile it runs directly.

Permission: `propose`

```sh
mazemail tag [OPTIONS] <ID>...
```

| Argument | Description |
| -------- | ----------- |
| `--add <TAG>...` | Tag to add (repeatable). |
| `--remove <TAG>...` | Tag to remove (repeatable). |
| `<ID>...` | Message (`m_…`) or thread (`t_…`) ids. |

Output (`--json`):

```ts
{
  affected: string[],  // Message ids affected.
  outcome: "applied" | "proposed",  // Applied directly, or a proposal.
  proposal?: string | null,  // The proposal's id, if proposed.
}
```

Examples:

```sh
# Retag a message
mazemail tag --add 'To Do' --remove Later m_8f3a2c
```

### `mazemail flag`

Flag or unflag messages.

Under the triage profile this becomes a proposal that you accept or reject in Maze's review UI; under the writer profile it runs directly.

Permission: `propose`

```sh
mazemail flag [OPTIONS] <ID>...
```

| Argument | Description |
| -------- | ----------- |
| `--clear` | Remove the flag instead. |
| `<ID>...` | Message (`m_…`) or thread (`t_…`) ids. |

Output (`--json`):

```ts
{
  affected: string[],  // Message ids affected.
  outcome: "applied" | "proposed",  // Applied directly, or a proposal.
  proposal?: string | null,  // The proposal's id, if proposed.
}
```

Examples:

```sh
# Flag a message
mazemail flag m_8f3a2c
```

### `mazemail read`

Mark messages read or unread.

Under the triage profile this becomes a proposal that you accept or reject in Maze's review UI; under the writer profile it runs directly.

Permission: `propose`

```sh
mazemail read [OPTIONS] <ID>...
```

| Argument | Description |
| -------- | ----------- |
| `--unread` | Mark as unread instead. |
| `<ID>...` | Message (`m_…`) or thread (`t_…`) ids. |

Output (`--json`):

```ts
{
  affected: string[],  // Message ids affected.
  outcome: "applied" | "proposed",  // Applied directly, or a proposal.
  proposal?: string | null,  // The proposal's id, if proposed.
}
```

Examples:

```sh
# Mark a thread unread
mazemail read --unread t_19bd
```

### `mazemail propose`

Propose a batch of actions for review.

Sends a list of actions (archive, move, tag, flag, mark_read, unsubscribe) that shows up grouped in Maze's review UI, where you accept or reject each item. Nothing changes until you accept.

Permission: `draft`

```sh
mazemail propose [OPTIONS] --file <FILE>
```

| Argument | Description |
| -------- | ----------- |
| `--file <FILE>` | JSON file with the actions (an array, or `{ "title": …, "actions": [...] }`), `-` for stdin. Required. |
| `--title <TITLE>` | Title shown above the proposal in Maze. |

Output (`--json`):

```ts
{
  items: integer,  // Number of items.
  proposal: string,  // The proposal's id.
}
```

Examples:

```sh
# Propose actions from a file
mazemail propose --title 'Inbox triage' --file actions.json
```

### `mazemail rule add`

Add a filter rule.

Adds a rule that runs on new mail. Always asks for approval in Maze.

Permission: `ask`

```sh
mazemail rule add [OPTIONS] --name <NAME> --when <QUERY> --then <ACTION>...
```

| Argument | Description |
| -------- | ----------- |
| `--name <NAME>` | Name shown in the rules list. Required. |
| `--when <QUERY>` | Which messages the rule matches, as a search query. Required. |
| `--then <ACTION>...` | What to do (repeatable): archive, read, flag, move:&lt;folder-id&gt;, tag:&lt;name&gt;. Required. |
| `--account <ACCOUNT>` | Limit the rule to one account. |
| `--apply-now` | Also run the rule on existing messages now. |

Output (`--json`):

```ts
{
  id: string,  // Rule id.
  name: string,  // Name.
}
```

Examples:

```sh
# Tag and archive receipts
mazemail rule add --name Receipts --when 'from:receipts@shop.example' --then tag:Receipts --then archive
```

### `mazemail send`

Send a draft.

Disabled by default. A token approved with --allow-send may use it, and then every send waits for your approval in Maze ("waiting for approval in Maze…") and is never approved automatically.

Permission: `disabled`

```sh
mazemail send <DRAFT>
```

| Argument | Description |
| -------- | ----------- |
| `<DRAFT>` | The draft's id (`d_…`). |

Output (`--json`):

```ts
{
  draft: string,  // The sent draft.
  outbox: string,  // Outbox item id (sending happens in the background).
}
```

Examples:

```sh
# Send after approval
mazemail send d_42
```

### `mazemail export`

Export messages to mbox or Maildir.

Writes the matching messages as plaintext to --to. Always asks for approval in Maze, because the export leaves encrypted storage.

Permission: `ask`

```sh
mazemail export [OPTIONS] --to <PATH> <QUERY>
```

| Argument | Description |
| -------- | ----------- |
| `<QUERY>` | Which messages, as a search query (`in:inbox`, `tag:receipts after:2026-01-01`). |
| `--to <PATH>` | Where to write (relative to the current directory). The export is plaintext. Required. |
| `--format <FORMAT>` | Format. One of `mbox`, `maildir`. Default: `mbox`. |

Output (`--json`):

```ts
{
  messages: integer,  // Messages exported.
  path: string,  // Where the export was written.
}
```

Examples:

```sh
# Export this year's inbox
mazemail export 'in:inbox after:2026-01-01' --to inbox.mbox
```

### `mazemail import`

Import an mbox file, a Maildir or a Thunderbird profile.

Imports into Local Folders or --account. Always asks for approval in Maze.

Permission: `ask`

```sh
mazemail import [OPTIONS] <PATH>
```

| Argument | Description |
| -------- | ----------- |
| `<PATH>` | File or directory to import (relative to the current directory). |
| `--format <FORMAT>` | Format (detected when omitted). One of `mbox`, `maildir`, `thunderbird`. |
| `--account <ACCOUNT>` | Account to import into (default: Local Folders). |

Output (`--json`):

```ts
{
  folders: integer,  // Folders created.
  messages: integer,  // Messages imported.
}
```

Examples:

```sh
# Import an mbox file
mazemail import ~/old-mail.mbox
```

### `mazemail account list`

List mail accounts and their sync state.

Lists the accounts in your token's scope with their server, sign-in method and sync state (up to date, syncing, offline, sign-in required).

Permission: `read`

```sh
mazemail account list
```

Output (`--json`):

```ts
[
  {
    auth?: string | null,  // `password`, `oauth:google` or `oauth:microsoft`, for synced accounts.
    email?: string | null,  // The address, if known.
    id: string,  // Account id.
    kind: string,  // `imap`, `gmail`, `m365` or `local`.
    name: string,  // The name in the folder pane.
    server?: string | null,  // The IMAP server (`host:port`), for synced accounts.
    state: string,  // `connecting`, `idle`, `syncing`, `offline`, `auth_required`, `error` or `not_synced`.
    status: string,  // The sync state as a sentence.
  }
]
```

Examples:

```sh
# Accounts that need a new sign-in
mazemail account list --json | jq -r '.[] | select(.state == "auth_required") | .id'
```

### `mazemail account add`

Add an IMAP, Gmail or Microsoft account.

Maze connects, logs in and lists the folders before it saves anything, then keeps the password (or the sign-in) in its encrypted storage and starts syncing. The password is read from the terminal without echo, or from stdin with --password-stdin; it is never a command-line argument. With --oauth google or --oauth microsoft, Maze opens the browser for the sign-in instead (Google needs auth.google.client_id in settings.json). Host, port and security are detected for well-known providers; so is the SMTP server for sending (override it with --smtp-host, --smtp-port and --smtp-security; it logs in like IMAP). Needs a writer token, and you approve every account in Maze.

Permission: `admin`

```sh
mazemail account add [OPTIONS] <EMAIL>
```

| Argument | Description |
| -------- | ----------- |
| `<EMAIL>` | The account's address. |
| `--name <NAME>` | The name in the folder pane (default: the address). |
| `--oauth <OAUTH>` | Sign in with Google or Microsoft in the browser (no password). One of `google`, `microsoft`. |
| `--host <HOST>` | IMAP host (default: detected from the address). |
| `--port <PORT>` | IMAP port (default: 993 for TLS, 143 otherwise). |
| `--security <SECURITY>` | Transport security. One of `tls`, `starttls`, `none`. |
| `--username <USERNAME>` | Login name (default: the address). |
| `--password-stdin` | Read the password from stdin (first line) instead of prompting on the terminal. |
| `--smtp-host <SMTP_HOST>` | SMTP host for sending (default: derived from the address and the IMAP host). |
| `--smtp-port <SMTP_PORT>` | SMTP port (default: 465 for TLS, 587 for STARTTLS). |
| `--smtp-security <SMTP_SECURITY>` | SMTP transport security. One of `tls`, `starttls`, `none`. |

Output (`--json`):

```ts
{
  auth?: string | null,  // `password`, `oauth:google` or `oauth:microsoft`, for synced accounts.
  email?: string | null,  // The address, if known.
  id: string,  // Account id.
  kind: string,  // `imap`, `gmail`, `m365` or `local`.
  name: string,  // The name in the folder pane.
  server?: string | null,  // The IMAP server (`host:port`), for synced accounts.
  state: string,  // `connecting`, `idle`, `syncing`, `offline`, `auth_required`, `error` or `not_synced`.
  status: string,  // The sync state as a sentence.
}
```

Examples:

```sh
# Add an account; asks for the password
mazemail account add alice@example.org --host imap.example.org
# Password from stdin (scripts)
printf '%s\n' "$PASSWORD" | mazemail account add alice@example.org --password-stdin
# Sign in with Google in the browser
mazemail account add bob@gmail.com --oauth google
```

### `mazemail account remove`

Remove a mail account.

Stops syncing the account and deletes its local mail, folders, identities and saved password or sign-in. Nothing changes on the server. Needs a writer token, and you approve it in Maze.

Permission: `admin`

```sh
mazemail account remove <ACCOUNT>
```

| Argument | Description |
| -------- | ----------- |
| `<ACCOUNT>` | The account (`a_…`). |

Output (`--json`):

```ts
{
  id: string,  // The removed account's id.
  removed: boolean,  // Always `true`.
}
```

Examples:

```sh
# Remove an account
mazemail account remove a_b
```

### `mazemail sync`

Sync accounts now.

Asks every account in your token's scope (or --account) to look for new mail and replay pending changes now, and retries at once when an account is offline. Returns right away; `mazemail account list` shows the progress.

Permission: `read`

```sh
mazemail sync [OPTIONS]
```

| Argument | Description |
| -------- | ----------- |
| `--account <ACCOUNT>` | Only this account (default: every account in scope). |

Output (`--json`):

```ts
{
  accounts: string[],  // The accounts asked to sync now.
}
```

Examples:

```sh
# Sync every account
mazemail sync
# Sync one account
mazemail sync --account a_b
```

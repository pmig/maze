# ADR-019: IMAP only, no Local Folders and no POP

- **Status:** Accepted
- **Date:** 2026-10-26

## Context

Maze followed Thunderbird in having two kinds of mailboxes: server accounts (IMAP,
Gmail and Microsoft 365 over IMAP) and *Local Folders*, a local account (`kind =
'local'`) that never syncs. Local Folders received mbox and Maildir imports, and the
Thunderbird importer copied Thunderbird's Local Folders and the downloaded mail of
POP3 accounts into such accounts (CONCEPT §17, PLAN §4.14).

Every local account is a second model running next to the synced one: folders without
a server name, commands and moves that have no server to replay to, mail that exists
nowhere but in one encrypted store (and is lost with its key, see
[security.md](../security.md)), backups that are the user's problem, and an importer
that has to tell POP3 stores, Local Folders and IMAP caches apart. POP3 itself was never
supported for receiving; offering its downloaded mail as local folders made it look
like it was.

The product decision: "For all users it is way simpler if we don't offer POP and Local
Folders. The Thunderbird import can stay."

## Decision

Maze is IMAP only, and every message lives in a server account.

- There are no local accounts any more. Nothing creates one (the store refuses), the
  importers don't, and the only code left for them reads the remaining legacy accounts
  (below). POP3 is neither offered nor mentioned.
- `mazemail import <file.mbox|maildir>` uploads into a folder of an IMAP account:
  `--account` picks the account (optional only when there is exactly one), `--folder`
  a slash-separated path below it (created on the server if missing; by default a
  top-level folder named after the file or directory). Maildir++ subfolders become
  subfolders. Every message becomes an IMAP `APPEND` with its flags and keywords,
  queued as a pending op and replayed by the sync engine, so the import works offline
  and retries like any other change. The messages show up after the folder's next
  sync. A re-run skips messages already in the target folder (by Message-ID; without
  one, by date, sender and subject) or already queued for upload (same bytes).
- The Thunderbird import keeps accounts, identities, signatures, SMTP servers, tags,
  filters (stored disabled), saved searches, address books, the layout suggestion and
  the offline body reuse for IMAP accounts. It skips POP3 accounts and Local Folders,
  each with an entry in the import result that says what to do instead, and skips the
  filters of skipped accounts.
- `mazemail export` stays: it's how mail leaves Maze, and the way out for existing
  Local Folders.

For **existing local accounts** we considered (a) keeping the rows while the app hides
them, with a notice and an export path, and (b) deleting them, but only if they are
empty. We do both, without hiding anything:

- Store migration 0010 deletes local accounts that hold no messages.
- Local accounts that still hold messages are kept, never synced, still visible,
  searchable and exportable. At every start Maze shows a notification: Local Folders
  are no longer supported; export them with
  `mazemail export 'account:"<name>"' --to <file>.mbox` and import the file into an
  IMAP account with `mazemail import <file>.mbox --account <account>`; they will be
  removed in a later version.

User mail is never deleted silently.

## Consequences

- One model: every folder has a server name, every change replays to a server, and
  losing the storage key means syncing again rather than losing mail.
- Importing mbox or Maildir needs an IMAP account and the network to complete, and is
  not instant: messages appear after the upload and the next sync, not when the command
  returns.
- Maze's `APPEND` sends no date, so the server's internal date of an uploaded message
  is the upload time; Maze sorts and shows mail by its `Date` header, so that only shows
  in other clients that sort by arrival.
- `APPEND` is not idempotent: if Maze stops between the server's `OK` and removing the
  op, the message is uploaded twice. Once the folder has synced, the next import run's
  Message-ID check recognizes the message, so a re-run never adds a third copy.
- A pending `APPEND` must keep its message blob alive until the server has it.
  Migration 0010 adds `pending_op.blob_id` with the same reference-count triggers as
  the other blob references, so garbage collection keeps the blob until the op is done.
- The store keeps parsing `kind = 'local'` as a legacy variant so the remaining Local
  Folders stay readable; the `CHECK` constraint keeps the value. A later version
  removes the variant together with the remaining local accounts.
- Users who relied on Local Folders as an offline archive move that mail into a folder
  on their server (or keep an export).

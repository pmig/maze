# ADR-026: Moving mail between accounts

- **Status:** Accepted
- **Date:** 2026-11-03

## Context

Quick Move only offered the folders of the mail's own account, and the core refused a
move into another account's folder. People keep several accounts (work, private, an old
provider) and want to move mail between them, as Thunderbird does. IMAP has no command
for that: `MOVE` and `COPY` work within one server. A move between accounts has to
upload the message to the other server (`APPEND`) and delete it on this one.

Maze changes mail offline first: a command applies locally and queues pending ops that
each account's sync engine replays in order (PLAN §4.5). A move between accounts
touches two queues, and the order matters: deleting the original before the other
server accepted the copy can lose mail. Old mail often has only its text stored
(`offline: "recent"`), so the full message may have to come from the source server.

## Decision

A move into another account's folder is a chain of three pending ops in two queues:

1. `move_to_account` (source account) reads each message: the stored full body, or
   `UID FETCH BODY.PEEK[]` from the server. When it is done it queues one
   `append_moved` per message in the target account's queue; that op holds the
   message's blob.
2. `append_moved` (target account) uploads the message with its flags and keywords.
   When it is done it queues a permanent `delete` of the original copies in the source
   account's queue.
3. `delete` (source account) removes the originals, as for any other delete.

Each op queues the next one in the same store write that removes it, and the engine
that finishes an op wakes the engine of the account it queued into. Locally the
message leaves the source folder at once; the copy appears in the target folder with
that folder's next sync. On Gmail, removing the original from a label removes the
label; a message only in All Mail is deleted.

Quick Move lists the folders of every account: the mail's own account first, then the
other accounts with their name in front (`Home / Archives`).

## Consequences

- A failure or a crash between the steps can leave a message in both accounts, never
  in neither. An upload the server refuses retries like any op and then shows as stuck
  in the activity log; the original stays until the copy is accepted.
- A move between accounts can't be undone with `cmd-z`: the copy in the other account
  is a new message. Moving it back is another move.
- The copy is a new message in the target account: its thread and tags there come from
  the target's own sync; flags and keywords travel with it.
- `append_moved` is not idempotent, like `append`: if Maze dies between the server's
  `OK` and removing the op, the message is uploaded twice.
- Copying between accounts and moving a selection that spans accounts are still not
  supported.

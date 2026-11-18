# ADR-029: Message rules

- **Status:** Accepted
- **Date:** 2026-11-18

## Context

People coming from Thunderbird have message filters, and the Thunderbird import already
stores them (CONCEPT §14, PLAN §4.13) as disabled rows of the `rule` table, because
nothing could run them. Without rules, newsletters, receipts and mailing lists pile up in
the Inbox, which is the first thing a Thunderbird user notices. Rules change mail on
their own, long after anyone approved them, so they have to be as safe and as
traceable as an agent's commands, and run exactly once per message even though sync
sees the same mail again after a restart, a reconnect or a UIDVALIDITY change.

## Decision

**Where.** Rules run in `mazemail_core`. The `rules` crate (L1, no IO) owns the stored
format (`condition_json`, `actions_json`), matching and the plan of actions;
`mazemail_core::rules` runs the plan as ordinary `MailCommand`s with
`Origin::Rule { rule }`, through `commands::apply_in_tx`, so pending IMAP ops, the
optimistic local change and the audit entry (origin `rule:<id>`) are exactly those of a
command from the UI. Rules never write mail state around that path.

**Triggers.** Rules with an arrival trigger (`arrival`, `arrival_before_junk`; Maze has
no junk classifier, so both run at the same point) run on new mail in the Inbox of
their account, or of every account when `account_id` is NULL. The sync engine signals
each account batch; a runner task in the core then processes that account. "Run rules
now" (`mazemail rule run`, Run Rules on Folder in the folder pane) runs the enabled
rules of a folder's account, or one named rule even if disabled, over every message in
a folder. `manual`, `after_send` and `periodic` rules only run on request.

**New mail, once.** Each watched folder (the Inbox; on Gmail, All Mail, for messages
with the Inbox label) has a cursor in `rule_cursor`: the highest UID the rules have
seen, per UIDVALIDITY. Messages above it are new. A missing or outdated cursor starts at
the end of what the first complete sync found (`uidnext - 1`), so existing mail is never
filtered by accident. Processed messages are recorded in `rule_message`, so a message
moved back into the Inbox (new UID) isn't filtered again. Picking the messages, running
the actions, recording them and advancing the cursor happen in one store write job:
they commit together. Each action runs in a nested savepoint; one that fails is rolled
back alone and logged (`rule_failed` or `rule_skipped`).

**Conditions.** `match` is `all`, `any` or `all_messages`; terms compare `from`, `to`,
`cc`, `to_or_cc`, `any_address`, `subject`, `body` (the stored preview), `list_id` and
`has_attachment` with `contains`, `not_contains`, `is`, `is_not`, `begins_with` and
`ends_with`, ignoring case. A rule with a `body` term holds a message back until its
body is downloaded (the runner asks for it), at most five minutes.

**Actions and order.** `mark_read`, `mark_unread`, `mark_flagged`, `add_tag`,
`copy_to_folder`, `move_to_folder`, `archive`, `trash`, `junk` and `stop`. Per rule,
flags and tags run first, then copies, then the first action that moves the message
(Thunderbird's order). Rules run by `ordinal`; `stop`, or a move, ends the evaluation
for that message. Folders are stored as paths in the message's account, tags as IMAP
keywords. `trash` never deletes for good: messages already in the Trash are skipped.

**Safety.** Imported filters stay disabled until the user enables them. A rule with a
condition Maze can't evaluate (Thunderbird's age, size, priority, regular expressions,
custom headers other than `List-Id`, grouped and/or) can't be enabled; unsupported
actions (reply, forward, priority, thread watching, add-on actions) are skipped and
listed as problems. Through the local API, `rule list` and `rule show` are reads;
`rule add`, `enable`, `disable`, `delete` and `run` are `ask`, because a rule keeps
changing mail after the token is gone. A token sees only rules of accounts in its scope
and global rules; only a token for every account may add a global rule.

**Deferred.** A rule editor in the app (the Rules tab lists and toggles; `mazemail rule
add` creates), reordering, editing conditions from the CLI, `after_send` and `periodic`
triggers, forwarding and replying, custom headers beyond `List-Id`, regular
expressions, size and age conditions, junk classification, and rules on other folders
than the Inbox.

## Consequences

- Imported Thunderbird filters can run, with the same audit trail and offline behavior
  as the user's own commands.
- The schema gains `rule_cursor` and `rule_message` (migration 0002). `rule_message`
  grows with the Inbox; its rows go with their messages.
- Mail that arrived before a rule was enabled, or before an account's first complete
  sync, is never filtered automatically; "Run rules now" covers it.
- Rule actions are not undoable with `cmd-z` (only the user's own last command is).
- The `body` condition sees the preview, not the whole text, and may wait for the body
  download; filters that need more than the beginning of a message match less often
  than in Thunderbird.

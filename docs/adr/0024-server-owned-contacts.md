# ADR-024: Server-owned contacts with a read-only local copy

- **Status:** Accepted
- **Date:** 2026-10-30

## Context

CONCEPT §1 lists "CalDAV and CardDAV, and syncing contacts with Google or Microsoft"
as a non-goal: Maze is mail only, with a local address book (imported once from
Thunderbird, plus collected addresses) used for autocomplete. In practice the people
you write to live in Google Contacts and Outlook: an address book imported once goes
stale, new colleagues are missing, and the Thunderbird profile Maze has to replace
(CONCEPT §2) syncs Google contacts. The composer also needs more than a plain text
field to be pleasant: recipients as chips, completion as you type, moving a recipient
from To to Cc by dragging.

Two-way sync is where contact features usually go wrong: a local edit and a server edit
of the same contact conflict, offline edits pile up in a queue, and the copies drift
apart. The store has the `contact` table (`book`, name, address, frequency, last use,
vCard) and a `bump_contact` that nothing called. The details and the order of the work
are in PLAN-contacts.

## Decision

- **The server owns the contacts.** Google (People API), Microsoft (Graph) and later
  CardDAV hold the truth. Maze keeps an encrypted, **read-only copy** in its database
  (the `contact` table, one `book` per account and address book) for autocomplete,
  the Address Book space and offline use; nothing is looked up live while you type.
- **Writes go to the server first.** Creating or editing a contact (from a message, or
  the New Contact modal) sends the change to the account's server; the local copy is
  only updated from the server's answer (etag-checked; a conflict reloads and shows the
  current version). There is no local-only edit and no offline queue: without a
  connection, saving says it can't save now. The copy can't drift.
- **Addresses you write to** are collected locally (the `collected` book: frequency and
  last use, bumped when a message is sent), as before; they are never uploaded.
  Completion offers address book entries first, then collected addresses.
- **Address Book space:** the reserved entry in the spaces bar (CONCEPT §8.2). All
  Contacts is one view across accounts, each row labeled with its account (name and
  color); the same person in several accounts is one row with several labels, and an
  edit always targets one account's version (no cross-account merge). "Other contacts"
  (Google's auto-saved ones) are read-only, with "Add to My Contacts".
- **Avatars:** photos of your own contacts are shown by default; Gravatar is opt-in
  (it tells a third party which addresses you look at).
- **Agents and the CLI** may read contacts as the token's permissions allow, but may
  only **propose** contact changes: every create or edit from an agent is a proposal
  you accept or reject per item, and accepting writes through to the server like an
  edit in the app.

## Consequences

- Contact sync needs new OAuth scopes (Google `contacts` and `contacts.other.readonly`
  with the People API enabled in the user's own Google Cloud project; Microsoft
  `Contacts.ReadWrite`, a second token per account because Graph is a separate
  resource). Users bring their own OAuth apps, so existing accounts have to consent
  again once, and each user has to change their app's setup; an account without the new
  scopes keeps working for mail with contacts off. `auth` needs per-resource tokens
  and a "missing scope" re-consent reason.
- Contacts become data on disk Maze didn't have before: names, addresses, phone numbers
  and photos of other people. They are in the encrypted database and blob cache like
  mail (ADR-012); restricted local API tokens keep seeing only what they see today.
- Without a connection you can't create or edit a contact. Autocomplete and avatars
  keep working from the copy.
- A new L1 crate (`contacts`) with a source per provider, sync tokens and etags per
  book; full resyncs when a sync token expires (Google: after 7 days).
- CardDAV providers (Fastmail, iCloud, mailbox.org, …) come after Google and Microsoft;
  until then their users have the imported address book and collected addresses.
- Calendars (CalDAV) stay a non-goal.

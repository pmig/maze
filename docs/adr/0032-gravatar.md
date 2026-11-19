# ADR-032: Gravatar, off by default

- **Status:** Accepted
- **Date:** 2026-11-19

## Context

Avatars in the Address Book and on the composer's recipient chips show the contact's
photo from Google or Microsoft ([ADR-024](0024-server-owned-contacts.md)) and the
initials otherwise. Collected addresses, imported books and contacts without a photo
are initials only. Many people registered a picture for their address at Gravatar,
which hands it out by the address's hash: `https://gravatar.com/avatar/<sha256>`.

Asking Gravatar tells a third party (Automattic) about the user's correspondents. The
hash hides little: anyone with a list of addresses can hash them and match. Maze's
other lookups avoid third parties on purpose: favicons come from each domain's own
website, never from an icon service ([security.md](../security.md#favicons)), and the
synced photos come from the account's own provider, which already has the mail.

## Decision

Maze asks Gravatar only when the user turns it on: `contacts.gravatar` is `"off"` by
default, `"contacts"` asks for contacts without a photo of their own, meaning
addresses in an address book and addresses the user wrote to. Never for addresses the
user only received mail from, so a sender can't trigger a lookup. A synced photo
always wins.

The lookup sends the SHA-256 of the trimmed, lowercase address with `d=404` (no
generated pictures), through the same client as contact photos: no cookies, no
`Referer`, no credentials, a generic `User-Agent`. Each address is asked once and then
at most weekly; the answer, also "none", is kept in a `gravatar` table and the picture
in the encrypted blob cache, both pruned when the address stops being a contact.
Turning it off stops the requests and hides the pictures at once.

The UI loads every avatar's picture through one app-wide cache (`ui::contact_photos`)
with a loader the app installs over the core, so the Address Book and the composer
share it and the UI crates don't know where pictures come from.

## Consequences

- Contacts without a synced photo can have a picture, for users who accept what that
  tells Gravatar. The default doesn't change what Maze sends anywhere.
- The privacy docs list Gravatar next to favicons, with exactly what is sent.
- A Google Workspace or Microsoft 365 directory lookup for colleagues' photos would be
  the provider's own data, not a third party's; it needs another permission and is
  left for later.

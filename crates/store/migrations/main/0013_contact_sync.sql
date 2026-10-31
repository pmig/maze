-- Maze store, schema v13: contacts synced from an account's server (ADR-024). The
-- server owns them; this is the read-only copy. The people's addresses are rows of
-- `contact` (one row per address, book "google:<account uuid>" for My Contacts and
-- "google-other:<account uuid>" for Other contacts); these tables hold what sync needs
-- on top. Everything goes with the account.

-- Where each synced book stands.
CREATE TABLE contact_source (
    book           TEXT PRIMARY KEY NOT NULL,
    account_id     INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    -- "google" or "google-other" (the book name's prefix).
    kind           TEXT NOT NULL,
    -- The token of the next incremental sync; NULL: the next sync lists everything.
    sync_token     TEXT,
    last_full_sync INTEGER,
    last_sync      INTEGER,
    -- "ok", "missing_scope" (contacts need permission) or "error".
    state          TEXT NOT NULL DEFAULT 'ok'
                     CHECK (state IN ('ok', 'missing_scope', 'error')),
    error          TEXT
) WITHOUT ROWID;
CREATE INDEX contact_source_account ON contact_source (account_id);

-- One person on the server (Google: `people/c…`, `otherContacts/c…`).
CREATE TABLE contact_person (
    book          TEXT NOT NULL REFERENCES contact_source (book) ON DELETE CASCADE,
    resource_name TEXT NOT NULL,
    etag          TEXT,
    display_name  TEXT,
    -- The person's addresses, lowercase, one per line: an address two people share
    -- belongs to one of them in `contact`, and moves to the other when it leaves.
    emails        TEXT NOT NULL DEFAULT '',
    -- vCard 4.0 of the fields Maze read (also on the person's `contact` rows; kept
    -- here for people without an address).
    vcard         TEXT NOT NULL,
    -- The person's own photo (no generated placeholder); NULL: none.
    photo_url     TEXT,
    -- The sync run that last saw the person; a full sync removes the ones it didn't.
    seen          INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (book, resource_name)
) WITHOUT ROWID;

-- Which person a synced `contact` row belongs to (NULL for local books).
ALTER TABLE contact ADD COLUMN resource_name TEXT;
CREATE INDEX contact_person_rows ON contact (book, resource_name)
    WHERE resource_name IS NOT NULL;

-- A person's rows go with the person (and so with the book and the account).
CREATE TRIGGER contact_person_deleted AFTER DELETE ON contact_person
BEGIN
    DELETE FROM contact WHERE book = OLD.book AND resource_name = OLD.resource_name;
END;

-- Photos fetched into the encrypted blob cache, by URL. `found` 0 remembers that
-- there is no photo behind the URL; entries are refreshed after about 7 days.
CREATE TABLE contact_photo (
    url        TEXT PRIMARY KEY NOT NULL,
    fetched_at INTEGER NOT NULL,
    found      INTEGER NOT NULL CHECK (found IN (0, 1))
) WITHOUT ROWID;

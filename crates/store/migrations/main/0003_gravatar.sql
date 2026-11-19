-- Gravatar lookups (`contacts.gravatar`, ADR-032): when Maze last asked Gravatar for
-- the picture of one of your contacts' addresses, under which URL, and whether it had
-- one. The picture itself is in the encrypted blob cache under that URL, like a
-- contact photo. Rows of addresses that are no longer contacts are pruned with the
-- contact photos.
CREATE TABLE gravatar (
  email      TEXT PRIMARY KEY NOT NULL COLLATE NOCASE,
  url        TEXT NOT NULL,
  fetched_at INTEGER NOT NULL,
  is_found   INTEGER NOT NULL CHECK (is_found IN (0, 1))
) WITHOUT ROWID;

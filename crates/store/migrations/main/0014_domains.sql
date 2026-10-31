-- Maze store: the domain index (PLAN-contacts §3b).
--
-- Every address's registrable domain (Public Suffix List: `mail.eu.northwind.example`
-- is `northwind.example`), so contacts and mail can be connected by domain, and the
-- state of each domain's favicon (fetched from the domain itself).
--
-- - `domain`: one row per registrable domain (lowercase ASCII, punycode for IDN) seen
--   in mail, or whose favicon state is known. The counters are maintained by the
--   triggers below from `message_domain`, so they stay right when messages go
--   (expunge, purge, account removal cascade). `last_seen` is the newest message date
--   ever seen and is not lowered by deletes. Contact counts are not stored: the
--   `contact` table is small and they are derived when read. Whether a domain is a
--   free-mail provider is decided in code (`mazemail_mime::domain::is_personal`).
-- - `message_domain`: which domains a message involves, one row per message and
--   domain. `roles` is a bit set: 1 an address in From, 2 one in To or Cc, 4 the
--   message was sent by you (From is one of your identities) to an address there. Your
--   own identity addresses are not indexed.
-- - Favicons: `favicon_state` 0 unknown, 1 stored in the encrypted cache under the
--   random id `favicon_key`, 2 the domain has none (remembered until it is due again);
--   `favicon_checked` is when it was last fetched.
-- - `domain_backfill`: messages that existed before this migration are indexed in the
--   background, in id order up to `last_message` (`next_message` is the next to do).
--   Messages added later are indexed when they are stored.
--
-- This migration doesn't depend on any other one after 0012.

CREATE TABLE domain (
  id              INTEGER PRIMARY KEY,
  name            TEXT NOT NULL UNIQUE,
  message_count   INTEGER NOT NULL DEFAULT 0 CHECK (message_count >= 0),
  from_count      INTEGER NOT NULL DEFAULT 0 CHECK (from_count >= 0),
  sent_count      INTEGER NOT NULL DEFAULT 0 CHECK (sent_count >= 0),
  last_seen       INTEGER,
  favicon_state   INTEGER NOT NULL DEFAULT 0 CHECK (favicon_state IN (0, 1, 2)),
  favicon_checked INTEGER,
  favicon_key     BLOB
);

CREATE TABLE message_domain (
  domain_id  INTEGER NOT NULL REFERENCES domain (id) ON DELETE CASCADE,
  message_id INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  roles      INTEGER NOT NULL CHECK (roles > 0),
  PRIMARY KEY (domain_id, message_id)
) WITHOUT ROWID;
CREATE INDEX message_domain_message ON message_domain (message_id);

CREATE TRIGGER message_domain_insert AFTER INSERT ON message_domain
BEGIN
  UPDATE domain SET
    message_count = message_count + 1,
    from_count = from_count + (NEW.roles & 1 <> 0),
    sent_count = sent_count + (NEW.roles & 4 <> 0),
    last_seen = max(COALESCE(last_seen, 0),
                    COALESCE((SELECT date FROM message WHERE id = NEW.message_id), 0))
  WHERE id = NEW.domain_id;
END;

CREATE TRIGGER message_domain_delete AFTER DELETE ON message_domain
BEGIN
  UPDATE domain SET
    message_count = message_count - 1,
    from_count = from_count - (OLD.roles & 1 <> 0),
    sent_count = sent_count - (OLD.roles & 4 <> 0)
  WHERE id = OLD.domain_id;
END;

CREATE TABLE domain_backfill (
  id           INTEGER PRIMARY KEY CHECK (id = 1),
  next_message INTEGER NOT NULL,
  last_message INTEGER NOT NULL
);
INSERT INTO domain_backfill (id, next_message, last_message)
  SELECT 1, 1, COALESCE(max(id), 0) FROM message;

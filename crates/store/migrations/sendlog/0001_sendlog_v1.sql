-- Maze send log, schema v1 (PLAN §4.3, §4.15).
--
-- One row per message the SMTP server accepted, keyed by its Message-ID. This file is
-- never restored from a backup, so after a rollback the outbox can tell which items
-- already went out and never sends them twice.
CREATE TABLE sent (
  rfc_message_id TEXT PRIMARY KEY,
  sent_at        INTEGER NOT NULL,
  identity_id    INTEGER,
  outbox_id      INTEGER
) WITHOUT ROWID;

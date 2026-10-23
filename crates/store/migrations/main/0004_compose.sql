-- Maze store, schema v4: local drafts and the outbox envelope (PLAN §4.5, §4.10).
--
-- A draft is what the composer autosaves while you write: Markdown body, recipients as
-- JSON arrays of `{"name": …, "email": …}`, attachments as file references (their
-- content goes into the outbox blob when the message is sent). Like everything in this
-- database it is encrypted at rest (SQLCipher).
--
-- The outbox gets the SMTP envelope (sender and every recipient, Bcc included), since
-- the message blob must not carry the Bcc header.

CREATE TABLE draft (
  id               INTEGER PRIMARY KEY AUTOINCREMENT,
  identity_id      INTEGER REFERENCES identity (id) ON DELETE SET NULL,
  to_json          TEXT NOT NULL DEFAULT '[]',
  cc_json          TEXT NOT NULL DEFAULT '[]',
  bcc_json         TEXT NOT NULL DEFAULT '[]',
  subject          TEXT NOT NULL DEFAULT '',
  body             TEXT NOT NULL DEFAULT '',            -- Markdown
  in_reply_to      TEXT,
  references_json  TEXT NOT NULL DEFAULT '[]',
  attachments_json TEXT NOT NULL DEFAULT '[]',
  origin_json      TEXT,                                -- reply / reply all / forward of a message
  created          INTEGER NOT NULL,
  updated          INTEGER NOT NULL
);
CREATE INDEX draft_identity ON draft (identity_id) WHERE identity_id IS NOT NULL;
CREATE INDEX draft_updated ON draft (updated DESC);

ALTER TABLE outbox ADD COLUMN envelope_json TEXT;

-- Maze store, schema v1 (PLAN §4.3, CONCEPT §5.1).
--
-- Conventions:
-- - Timestamps are Unix seconds (UTC).
-- - `*_json` columns hold JSON text written by the store.
-- - Every child column with a foreign key has an index, so deletes of parents never scan.
-- - Ids are AUTOINCREMENT: public ids (`m_…`, `t_…`) are never reused for other rows.
-- - Blob reference counts are maintained by triggers (bottom of this file); a row can
--   only reference a blob that has a `blob` row (foreign key), and the store only
--   creates `blob` rows for files that are already on disk.

CREATE TABLE account (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  kind            TEXT NOT NULL CHECK (kind IN ('imap', 'gmail', 'm365', 'local', 'jmap')),
  name            TEXT NOT NULL,
  config_json     TEXT NOT NULL DEFAULT '{}',
  sync_state_json TEXT,
  ordinal         INTEGER NOT NULL DEFAULT 0
);

-- The password or OAuth token lives in the OS keychain, never here.
CREATE TABLE smtp_server (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT,
  host       TEXT NOT NULL,
  port       INTEGER,
  security   TEXT NOT NULL DEFAULT 'tls' CHECK (security IN ('tls', 'starttls', 'none')),
  auth       TEXT NOT NULL DEFAULT 'plain' CHECK (auth IN ('none', 'plain', 'login', 'oauth2')),
  username   TEXT,
  saves_sent TEXT NOT NULL DEFAULT 'auto' CHECK (saves_sent IN ('auto', 'yes', 'no'))
);

-- HTML and plain-text variants (CONCEPT §8.6).
CREATE TABLE signature (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  name      TEXT NOT NULL,
  body_html TEXT,
  body_text TEXT,
  ordinal   INTEGER NOT NULL DEFAULT 0,
  created   INTEGER NOT NULL,
  updated   INTEGER NOT NULL
);

CREATE TABLE folder (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id    INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  parent_id     INTEGER REFERENCES folder (id) ON DELETE CASCADE,
  remote_name   TEXT,
  display_name  TEXT NOT NULL,
  role          TEXT,
  is_label      INTEGER NOT NULL DEFAULT 0,          -- Gmail label = virtual folder
  uidvalidity   INTEGER,
  uidnext       INTEGER,
  highestmodseq INTEGER,
  sync_policy   TEXT,
  unread        INTEGER NOT NULL DEFAULT 0,          -- maintained by the writer
  total         INTEGER NOT NULL DEFAULT 0,          -- maintained by the writer
  ordinal       INTEGER NOT NULL DEFAULT 0,
  favorite      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX folder_account ON folder (account_id);
CREATE INDEX folder_parent ON folder (parent_id);
CREATE UNIQUE INDEX folder_remote ON folder (account_id, remote_name) WHERE remote_name IS NOT NULL;
CREATE INDEX folder_role ON folder (role) WHERE role IS NOT NULL;

CREATE TABLE identity (
  id                  INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id          INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  label               TEXT,
  display_name        TEXT,
  email               TEXT NOT NULL,
  reply_to            TEXT,
  signature_id        INTEGER REFERENCES signature (id) ON DELETE SET NULL,
  smtp_id             INTEGER REFERENCES smtp_server (id) ON DELETE SET NULL,
  sent_folder_id      INTEGER REFERENCES folder (id) ON DELETE SET NULL,
  drafts_folder_id    INTEGER REFERENCES folder (id) ON DELETE SET NULL,
  archive_folder_id   INTEGER REFERENCES folder (id) ON DELETE SET NULL,
  archive_granularity INTEGER NOT NULL DEFAULT 1,     -- 0 single folder, 1 yearly, 2 monthly
  color               TEXT,
  match_patterns_json TEXT,
  is_default          INTEGER NOT NULL DEFAULT 0,
  ordinal             INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX identity_account ON identity (account_id);
CREATE INDEX identity_signature ON identity (signature_id) WHERE signature_id IS NOT NULL;
CREATE INDEX identity_smtp ON identity (smtp_id) WHERE smtp_id IS NOT NULL;
CREATE INDEX identity_sent_folder ON identity (sent_folder_id) WHERE sent_folder_id IS NOT NULL;
CREATE INDEX identity_drafts_folder ON identity (drafts_folder_id) WHERE drafts_folder_id IS NOT NULL;
CREATE INDEX identity_archive_folder ON identity (archive_folder_id) WHERE archive_folder_id IS NOT NULL;

CREATE TABLE identity_rule (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  identity_id INTEGER NOT NULL REFERENCES identity (id) ON DELETE CASCADE,
  kind        TEXT NOT NULL CHECK (kind IN ('recipient', 'domain', 'folder', 'list')),
  pattern     TEXT NOT NULL,
  ordinal     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX identity_rule_identity ON identity_rule (identity_id, ordinal);

CREATE TABLE blob (
  id      BLOB PRIMARY KEY,
  size    INTEGER NOT NULL,                           -- uncompressed bytes
  created INTEGER NOT NULL,
  refs    INTEGER NOT NULL DEFAULT 0 CHECK (refs >= 0)
) WITHOUT ROWID;
CREATE INDEX blob_unreferenced ON blob (id) WHERE refs = 0;

CREATE TABLE thread (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id        INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  subject           TEXT,                             -- subject of the oldest message
  subject_key       TEXT,                             -- normalized subject, for threading
  last_date         INTEGER NOT NULL DEFAULT 0,
  last_message_id   INTEGER,
  message_count     INTEGER NOT NULL DEFAULT 0,
  unread_count      INTEGER NOT NULL DEFAULT 0,
  flagged           INTEGER NOT NULL DEFAULT 0,
  attachment_count  INTEGER NOT NULL DEFAULT 0,
  participants_json TEXT
);
-- Account-scope thread lists. The partial indexes make quick filters range scans; the
-- trailing columns make them covering (SQLite still evaluates the WHERE terms).
CREATE INDEX thread_list ON thread (account_id, last_date DESC, id DESC, message_count)
  WHERE message_count > 0;
CREATE INDEX thread_list_unread
  ON thread (account_id, last_date DESC, id DESC, message_count, unread_count)
  WHERE message_count > 0 AND unread_count > 0;
CREATE INDEX thread_list_flagged
  ON thread (account_id, last_date DESC, id DESC, message_count, flagged)
  WHERE message_count > 0 AND flagged > 0;
CREATE INDEX thread_list_attachments
  ON thread (account_id, last_date DESC, id DESC, message_count, attachment_count)
  WHERE message_count > 0 AND attachment_count > 0;
CREATE INDEX thread_subject ON thread (account_id, subject_key) WHERE subject_key IS NOT NULL;

CREATE TABLE message (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id         INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  blob_id            BLOB REFERENCES blob (id),       -- NULL until the body is fetched
  body_state         INTEGER NOT NULL DEFAULT 0,      -- 0 none, 1 text parts, 2 full raw
  rfc_message_id     TEXT,
  thread_id          INTEGER NOT NULL REFERENCES thread (id),
  in_reply_to        TEXT,
  references_json    TEXT,
  date               INTEGER NOT NULL,
  received           INTEGER,
  from_addr          TEXT,
  from_name          TEXT,
  to_json            TEXT,
  cc_json            TEXT,
  delivered_to       TEXT,
  subject            TEXT,
  snippet            TEXT,
  size               INTEGER,
  has_attachments    INTEGER NOT NULL DEFAULT 0,
  list_id            TEXT,
  bodystructure_json TEXT,
  gm_msgid           INTEGER,
  gm_thrid           INTEGER,
  auth_json          TEXT,
  identity_hint      INTEGER REFERENCES identity (id) ON DELETE SET NULL,
  detached_at        INTEGER                          -- set while the message has no location
);
CREATE INDEX message_thread ON message (thread_id, date);
CREATE INDEX message_rfcid ON message (rfc_message_id);
CREATE INDEX message_account ON message (account_id);
CREATE UNIQUE INDEX message_gmail ON message (account_id, gm_msgid) WHERE gm_msgid IS NOT NULL;
CREATE INDEX message_gm_thread ON message (account_id, gm_thrid) WHERE gm_thrid IS NOT NULL;
CREATE INDEX message_blob ON message (blob_id) WHERE blob_id IS NOT NULL;
CREATE INDEX message_identity ON message (identity_hint) WHERE identity_hint IS NOT NULL;
CREATE INDEX message_detached ON message (account_id, detached_at) WHERE detached_at IS NOT NULL;

-- IMAP flags and keywords belong to the copy in each mailbox (PLAN §4.3).
CREATE TABLE message_location (
  message_id    INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  folder_id     INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
  uid           INTEGER,                             -- NULL for Gmail labels and pending moves
  modseq        INTEGER,
  flags         INTEGER NOT NULL DEFAULT 0,
  keywords_json TEXT,                                -- lowercase IMAP keywords = tags
  PRIMARY KEY (folder_id, message_id)
) WITHOUT ROWID;
CREATE UNIQUE INDEX location_folder_uid ON message_location (folder_id, uid) WHERE uid IS NOT NULL;
-- Covers the per-message flag aggregation (effective flags, thread recompute).
CREATE INDEX location_message ON message_location (message_id, flags);

-- Message-ids a message refers to (References + In-Reply-To), so a late parent or a
-- sibling with a missing parent finds its thread.
CREATE TABLE message_ref (
  rfc_id     TEXT NOT NULL,
  message_id INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  PRIMARY KEY (rfc_id, message_id)
) WITHOUT ROWID;
CREATE INDEX message_ref_message ON message_ref (message_id);

-- One row per folder and thread; maintained by the writer.
CREATE TABLE folder_thread (
  folder_id        INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
  thread_id        INTEGER NOT NULL REFERENCES thread (id) ON DELETE CASCADE,
  last_date        INTEGER NOT NULL,
  message_count    INTEGER NOT NULL,
  unread_count     INTEGER NOT NULL,
  flagged          INTEGER NOT NULL,
  attachment_count INTEGER NOT NULL,
  PRIMARY KEY (folder_id, thread_id)
) WITHOUT ROWID;
-- Folder thread lists (keyset pagination). The partial indexes make quick filters range
-- scans; the trailing column makes them covering (SQLite still evaluates the WHERE term).
CREATE INDEX folder_thread_list ON folder_thread (folder_id, last_date DESC, thread_id DESC);
CREATE INDEX folder_thread_unread
  ON folder_thread (folder_id, last_date DESC, thread_id DESC, unread_count)
  WHERE unread_count > 0;
CREATE INDEX folder_thread_flagged
  ON folder_thread (folder_id, last_date DESC, thread_id DESC, flagged)
  WHERE flagged > 0;
CREATE INDEX folder_thread_attachments
  ON folder_thread (folder_id, last_date DESC, thread_id DESC, attachment_count)
  WHERE attachment_count > 0;
CREATE INDEX folder_thread_thread ON folder_thread (thread_id);

CREATE TABLE tag (
  id      INTEGER PRIMARY KEY AUTOINCREMENT,
  keyword TEXT NOT NULL UNIQUE,                      -- lowercase IMAP keyword
  name    TEXT NOT NULL,
  color   TEXT,
  ordinal INTEGER NOT NULL DEFAULT 0
);

-- Derived from the keywords of all locations of a message.
CREATE TABLE message_tag (
  message_id INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  tag_id     INTEGER NOT NULL REFERENCES tag (id) ON DELETE CASCADE,
  PRIMARY KEY (message_id, tag_id)
) WITHOUT ROWID;
CREATE INDEX message_tag_tag ON message_tag (tag_id, message_id);

CREATE TABLE pending_op (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id   INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  kind         TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  origin       TEXT NOT NULL,
  state        TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'stuck')),
  attempts     INTEGER NOT NULL DEFAULT 0,
  next_attempt INTEGER NOT NULL DEFAULT 0,
  last_error   TEXT,
  created      INTEGER NOT NULL
);
CREATE INDEX pending_op_account ON pending_op (account_id, id);

CREATE TABLE outbox (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  identity_id    INTEGER REFERENCES identity (id) ON DELETE SET NULL,
  blob_id        BLOB REFERENCES blob (id),
  rfc_message_id TEXT NOT NULL,
  send_at        INTEGER NOT NULL,
  state          TEXT NOT NULL DEFAULT 'queued'
                   CHECK (state IN ('queued', 'sending', 'sent', 'failed', 'frozen')),
  attempts       INTEGER NOT NULL DEFAULT 0,
  last_error     TEXT,
  created        INTEGER NOT NULL
);
CREATE INDEX outbox_due ON outbox (state, send_at);
CREATE INDEX outbox_identity ON outbox (identity_id) WHERE identity_id IS NOT NULL;
CREATE INDEX outbox_blob ON outbox (blob_id) WHERE blob_id IS NOT NULL;
CREATE INDEX outbox_rfcid ON outbox (rfc_message_id);

-- Local address book, collected addresses included. No sync.
CREATE TABLE contact (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  book         TEXT NOT NULL DEFAULT 'collected',
  display_name TEXT,
  email        TEXT NOT NULL COLLATE NOCASE,
  frequency    INTEGER NOT NULL DEFAULT 0,
  last_used    INTEGER,
  vcard        TEXT
);
CREATE UNIQUE INDEX contact_email ON contact (book, email);
CREATE INDEX contact_lookup ON contact (email COLLATE NOCASE);

-- Filters (CONCEPT §14). `account_id` NULL means every account.
CREATE TABLE rule (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id     INTEGER REFERENCES account (id) ON DELETE CASCADE,
  name           TEXT NOT NULL,
  ordinal        INTEGER NOT NULL DEFAULT 0,
  enabled        INTEGER NOT NULL DEFAULT 1,
  trigger_kind   TEXT NOT NULL
                   CHECK (trigger_kind IN ('arrival_before_junk', 'arrival', 'manual', 'after_send', 'periodic')),
  condition_json TEXT NOT NULL,
  actions_json   TEXT NOT NULL,
  created        INTEGER NOT NULL,
  updated        INTEGER NOT NULL
);
CREATE INDEX rule_account ON rule (account_id, ordinal);

-- Thunderbird virtual folders. `folders_json` NULL means all folders.
CREATE TABLE saved_search (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  name         TEXT NOT NULL,
  query        TEXT NOT NULL,
  folders_json TEXT,
  ordinal      INTEGER NOT NULL DEFAULT 0,
  created      INTEGER NOT NULL
);

-- Only the BLAKE3 hash of a token is stored (PLAN §4.11).
CREATE TABLE api_token (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  hash       BLOB NOT NULL UNIQUE,
  profile    TEXT NOT NULL,
  scope_json TEXT NOT NULL DEFAULT '{}',
  expires    INTEGER,
  label      TEXT,
  created    INTEGER NOT NULL,
  last_used  INTEGER,
  revoked    INTEGER
);

CREATE TABLE proposal (
  id       INTEGER PRIMARY KEY AUTOINCREMENT,
  token_id INTEGER REFERENCES api_token (id) ON DELETE SET NULL,
  title    TEXT NOT NULL,
  state    TEXT NOT NULL DEFAULT 'pending'
             CHECK (state IN ('pending', 'approved', 'rejected', 'partial', 'applied')),
  created  INTEGER NOT NULL,
  decided  INTEGER
);
CREATE INDEX proposal_state ON proposal (state, created);
CREATE INDEX proposal_token ON proposal (token_id) WHERE token_id IS NOT NULL;

CREATE TABLE proposal_item (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  proposal_id  INTEGER NOT NULL REFERENCES proposal (id) ON DELETE CASCADE,
  command_json TEXT NOT NULL,
  reason       TEXT,
  state        TEXT NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'approved', 'rejected', 'applied', 'failed'))
);
CREATE INDEX proposal_item_proposal ON proposal_item (proposal_id, id);

CREATE TABLE audit_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  at          INTEGER NOT NULL,
  origin      TEXT NOT NULL,
  action      TEXT NOT NULL,
  detail_json TEXT
);
CREATE INDEX audit_log_at ON audit_log (at);

CREATE TABLE agent_thread (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id        TEXT NOT NULL,
  title           TEXT,
  mail_thread_id  INTEGER REFERENCES thread (id) ON DELETE SET NULL,
  acp_session_id  TEXT,
  transcript_blob BLOB REFERENCES blob (id),
  created         INTEGER NOT NULL,
  updated         INTEGER NOT NULL
);
CREATE INDEX agent_thread_updated ON agent_thread (updated DESC);
CREATE INDEX agent_thread_mail ON agent_thread (mail_thread_id) WHERE mail_thread_id IS NOT NULL;
CREATE INDEX agent_thread_blob ON agent_thread (transcript_blob) WHERE transcript_blob IS NOT NULL;

-- Blob reference counts.
CREATE TRIGGER message_blob_insert AFTER INSERT ON message WHEN NEW.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER message_blob_update AFTER UPDATE OF blob_id ON message
  WHEN NEW.blob_id IS NOT OLD.blob_id
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.blob_id;
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER message_blob_delete AFTER DELETE ON message WHEN OLD.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.blob_id;
END;

CREATE TRIGGER outbox_blob_insert AFTER INSERT ON outbox WHEN NEW.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER outbox_blob_update AFTER UPDATE OF blob_id ON outbox
  WHEN NEW.blob_id IS NOT OLD.blob_id
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.blob_id;
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER outbox_blob_delete AFTER DELETE ON outbox WHEN OLD.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.blob_id;
END;

CREATE TRIGGER agent_thread_blob_insert AFTER INSERT ON agent_thread
  WHEN NEW.transcript_blob IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.transcript_blob;
END;
CREATE TRIGGER agent_thread_blob_update AFTER UPDATE OF transcript_blob ON agent_thread
  WHEN NEW.transcript_blob IS NOT OLD.transcript_blob
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.transcript_blob;
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.transcript_blob;
END;
CREATE TRIGGER agent_thread_blob_delete AFTER DELETE ON agent_thread
  WHEN OLD.transcript_blob IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.transcript_blob;
END;

-- Thunderbird's default tags, so `$label1`…`$label5` from the server show up as tags.
INSERT INTO tag (keyword, name, color, ordinal) VALUES
  ('$label1', 'Important', '#FF0000', 1),
  ('$label2', 'Work', '#FF9900', 2),
  ('$label3', 'Personal', '#009900', 3),
  ('$label4', 'To Do', '#3333FF', 4),
  ('$label5', 'Later', '#993399', 5);

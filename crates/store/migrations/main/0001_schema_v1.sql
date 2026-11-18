-- Maze store, schema v1 (PLAN §4.3, CONCEPT §5.1).
--
-- The one schema of the first release. It replaces the migrations of the development
-- builds before it (0001 to 0014); a database those builds left at their last schema
-- is converted in place when it is opened (`src/migrate/dev_v14.rs`). Later
-- changes are new migrations after this one.
--
-- Conventions:
-- - Timestamps are Unix seconds (UTC) in columns ending in `_at` (`message.date` is
--   the Date header, `last_date` the newest one of a thread).
-- - Flags start with `is_` or `has_`, counts end in `_count` and a foreign key is the
--   referenced table's name (or its role) followed by `_id`.
-- - `*_json` columns hold JSON text written by the store.
-- - Every child column with a foreign key has an index, so deletes of parents never scan.
-- - Ids are AUTOINCREMENT: public ids (`m_…`, `t_…`) are never reused for other rows.
-- - Blob reference counts are maintained by triggers (bottom of this file); a row can
--   only reference a blob that has a `blob` row (foreign key), and the store only
--   creates `blob` rows for files that are already on disk.

-- `kind` is the provider: plain IMAP, Gmail or Microsoft 365 (all IMAP, ADR-019).
--
-- `uuid` is stable for the account's lifetime and never reused: account secrets are
-- keyed by it (`account_secret`). The store generates it (UUID v4, lowercase
-- hyphenated text) when it creates an account; a row inserted without one gets one
-- from the trigger below, and once set it can't change.
CREATE TABLE account (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  kind            TEXT NOT NULL CHECK (kind IN ('imap', 'gmail', 'm365')),
  name            TEXT NOT NULL,
  config_json     TEXT NOT NULL DEFAULT '{}',
  sync_state_json TEXT,
  ordinal         INTEGER NOT NULL DEFAULT 0,
  uuid            TEXT NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX account_uuid ON account (uuid);

CREATE TRIGGER account_uuid_default AFTER INSERT ON account WHEN NEW.uuid = ''
BEGIN
  UPDATE account SET uuid =
    lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' ||
    substr(lower(hex(randomblob(2))), 2) || '-' ||
    substr('89ab', 1 + (abs(random()) % 4), 1) || substr(lower(hex(randomblob(2))), 2) || '-' ||
    lower(hex(randomblob(6)))
  WHERE id = NEW.id;
END;

CREATE TRIGGER account_uuid_immutable BEFORE UPDATE OF uuid ON account
WHEN OLD.uuid <> '' AND NEW.uuid IS NOT OLD.uuid
BEGIN
  SELECT RAISE(ABORT, 'account uuid is immutable');
END;

-- An account's secrets (IMAP password, OAuth refresh token, SMTP password; ADR-017),
-- encrypted like everything in this database under the master key, Maze's only
-- keychain item. `kind` is the `auth` crate's name (`password`, `oauth-refresh`,
-- `smtp-password`). Deleting an account deletes its secrets in the same transaction.
CREATE TABLE account_secret (
  account_uuid TEXT NOT NULL REFERENCES account (uuid) ON DELETE CASCADE,
  kind         TEXT NOT NULL,
  secret       TEXT NOT NULL,
  updated_at   INTEGER NOT NULL,
  PRIMARY KEY (account_uuid, kind)
) WITHOUT ROWID;

-- Agent sign-in secrets (ADR-017): an API key or token pasted into the agent column's
-- sign-in card, injected into that agent's environment as `env` when it starts and
-- never written to settings.json, the agent's home or a log. One per agent (its
-- registry id or custom id).
CREATE TABLE agent_secret (
  agent_id   TEXT PRIMARY KEY,
  env        TEXT NOT NULL,
  secret     TEXT NOT NULL,
  updated_at INTEGER NOT NULL
) WITHOUT ROWID;

-- The password lives in `account_secret`, never here.
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
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL,
  body_html  TEXT,
  body_text  TEXT,
  ordinal    INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE folder (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id    INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  parent_id     INTEGER REFERENCES folder (id) ON DELETE CASCADE,
  remote_name   TEXT,                                -- the IMAP mailbox name or Gmail label
  display_name  TEXT NOT NULL,
  role          TEXT,
  is_label      INTEGER NOT NULL DEFAULT 0,          -- Gmail label = virtual folder
  uidvalidity   INTEGER,
  uidnext       INTEGER,
  highestmodseq INTEGER,
  unread_count  INTEGER NOT NULL DEFAULT 0,          -- maintained by the writer
  total_count   INTEGER NOT NULL DEFAULT 0,          -- maintained by the writer
  ordinal       INTEGER NOT NULL DEFAULT 0,
  is_favorite   INTEGER NOT NULL DEFAULT 0
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
  smtp_server_id      INTEGER REFERENCES smtp_server (id) ON DELETE SET NULL,
  sent_folder_id      INTEGER REFERENCES folder (id) ON DELETE SET NULL,
  drafts_folder_id    INTEGER REFERENCES folder (id) ON DELETE SET NULL,
  archive_folder_id   INTEGER REFERENCES folder (id) ON DELETE SET NULL,
  archive_granularity INTEGER NOT NULL DEFAULT 1,     -- 0 single folder, 1 yearly, 2 monthly
  is_default          INTEGER NOT NULL DEFAULT 0,
  ordinal             INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX identity_account ON identity (account_id);
CREATE INDEX identity_signature ON identity (signature_id) WHERE signature_id IS NOT NULL;
CREATE INDEX identity_smtp_server ON identity (smtp_server_id) WHERE smtp_server_id IS NOT NULL;
CREATE INDEX identity_sent_folder ON identity (sent_folder_id) WHERE sent_folder_id IS NOT NULL;
CREATE INDEX identity_drafts_folder ON identity (drafts_folder_id) WHERE drafts_folder_id IS NOT NULL;
CREATE INDEX identity_archive_folder ON identity (archive_folder_id) WHERE archive_folder_id IS NOT NULL;

CREATE TABLE blob (
  id         BLOB PRIMARY KEY,
  size       INTEGER NOT NULL,                        -- uncompressed bytes
  created_at INTEGER NOT NULL,
  ref_count  INTEGER NOT NULL DEFAULT 0 CHECK (ref_count >= 0)
) WITHOUT ROWID;
CREATE INDEX blob_unreferenced ON blob (id) WHERE ref_count = 0;

CREATE TABLE thread (
  id                INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id        INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  subject           TEXT,                             -- subject of the oldest message
  subject_key       TEXT,                             -- normalized subject, for threading
  last_date         INTEGER NOT NULL DEFAULT 0,
  last_message_id   INTEGER,
  message_count     INTEGER NOT NULL DEFAULT 0,
  unread_count      INTEGER NOT NULL DEFAULT 0,
  flagged_count     INTEGER NOT NULL DEFAULT 0,
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
  ON thread (account_id, last_date DESC, id DESC, message_count, flagged_count)
  WHERE message_count > 0 AND flagged_count > 0;
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
  received_at        INTEGER,
  from_email         TEXT,
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
  identity_hint_id   INTEGER REFERENCES identity (id) ON DELETE SET NULL,
  detached_at        INTEGER                          -- set while the message has no location
);
CREATE INDEX message_thread ON message (thread_id, date);
CREATE INDEX message_rfcid ON message (rfc_message_id);
CREATE INDEX message_account ON message (account_id);
CREATE UNIQUE INDEX message_gmail ON message (account_id, gm_msgid) WHERE gm_msgid IS NOT NULL;
CREATE INDEX message_gm_thread ON message (account_id, gm_thrid) WHERE gm_thrid IS NOT NULL;
CREATE INDEX message_blob ON message (blob_id) WHERE blob_id IS NOT NULL;
CREATE INDEX message_identity ON message (identity_hint_id) WHERE identity_hint_id IS NOT NULL;
CREATE INDEX message_detached ON message (account_id, detached_at) WHERE detached_at IS NOT NULL;
-- Account-scope message lists.
CREATE INDEX message_list ON message (account_id, date DESC, id DESC) WHERE detached_at IS NULL;

-- IMAP flags and keywords belong to the copy in each mailbox (PLAN §4.3).
--
-- Each location also carries its message's date and attachment flag, so the unthreaded
-- view reads a folder's messages in date order (and filters by attachment) straight
-- from an index. The writer fills both when it adds a location; the triggers below keep
-- them right when the message changes.
CREATE TABLE message_location (
  message_id      INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  folder_id       INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
  uid             INTEGER,                           -- NULL for Gmail labels and pending moves
  modseq          INTEGER,
  flags           INTEGER NOT NULL DEFAULT 0,
  keywords_json   TEXT,                              -- lowercase IMAP keywords = tags
  date            INTEGER NOT NULL DEFAULT 0,
  has_attachments INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (folder_id, message_id)
) WITHOUT ROWID;
CREATE UNIQUE INDEX location_folder_uid ON message_location (folder_id, uid) WHERE uid IS NOT NULL;
-- Covers the per-message flag aggregation (effective flags, thread recompute).
CREATE INDEX location_message ON message_location (message_id, flags);
-- Folder message lists. The partial indexes make the unread, starred and attachment
-- quick filters range scans (SQLite still evaluates the WHERE term).
CREATE INDEX location_list ON message_location (folder_id, date DESC, message_id DESC);
CREATE INDEX location_list_unread ON message_location (folder_id, date DESC, message_id DESC)
  WHERE flags & 1 = 0;
CREATE INDEX location_list_flagged ON message_location (folder_id, date DESC, message_id DESC)
  WHERE flags & 4 <> 0;
CREATE INDEX location_list_attachments
  ON message_location (folder_id, date DESC, message_id DESC)
  WHERE has_attachments = 1;

-- Message-ids a message refers to (References + In-Reply-To), so a late parent or a
-- sibling with a missing parent finds its thread.
CREATE TABLE message_ref (
  rfc_message_id TEXT NOT NULL,
  message_id     INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  PRIMARY KEY (rfc_message_id, message_id)
) WITHOUT ROWID;
CREATE INDEX message_ref_message ON message_ref (message_id);

-- One row per folder and thread; maintained by the writer.
CREATE TABLE folder_thread (
  folder_id        INTEGER NOT NULL REFERENCES folder (id) ON DELETE CASCADE,
  thread_id        INTEGER NOT NULL REFERENCES thread (id) ON DELETE CASCADE,
  last_date        INTEGER NOT NULL,
  message_count    INTEGER NOT NULL,
  unread_count     INTEGER NOT NULL,
  flagged_count    INTEGER NOT NULL,
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
  ON folder_thread (folder_id, last_date DESC, thread_id DESC, flagged_count)
  WHERE flagged_count > 0;
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

-- Queued server changes. An `append` op (`mazemail import mbox|maildir` uploads into
-- an IMAP folder) is the only reference to its message until the server has it, so
-- `blob_id` holds the blob, counted like every other blob reference.
CREATE TABLE pending_op (
  id              INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id      INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  kind            TEXT NOT NULL,
  payload_json    TEXT NOT NULL,
  origin          TEXT NOT NULL,
  state           TEXT NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'stuck')),
  attempt_count   INTEGER NOT NULL DEFAULT 0,
  next_attempt_at INTEGER NOT NULL DEFAULT 0,
  last_error      TEXT,
  created_at      INTEGER NOT NULL,
  blob_id         BLOB REFERENCES blob (id)
);
CREATE INDEX pending_op_account ON pending_op (account_id, id);
CREATE INDEX pending_op_blob ON pending_op (blob_id) WHERE blob_id IS NOT NULL;

-- `envelope_json` is the SMTP envelope (sender and every recipient, Bcc included):
-- the message blob must not carry the Bcc header.
CREATE TABLE outbox (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  identity_id    INTEGER REFERENCES identity (id) ON DELETE SET NULL,
  blob_id        BLOB REFERENCES blob (id),
  rfc_message_id TEXT NOT NULL,
  send_at        INTEGER NOT NULL,
  state          TEXT NOT NULL DEFAULT 'queued'
                   CHECK (state IN ('queued', 'sending', 'sent', 'failed', 'frozen')),
  attempt_count  INTEGER NOT NULL DEFAULT 0,
  last_error     TEXT,
  created_at     INTEGER NOT NULL,
  envelope_json  TEXT
);
CREATE INDEX outbox_due ON outbox (state, send_at);
CREATE INDEX outbox_identity ON outbox (identity_id) WHERE identity_id IS NOT NULL;
CREATE INDEX outbox_blob ON outbox (blob_id) WHERE blob_id IS NOT NULL;
CREATE INDEX outbox_rfcid ON outbox (rfc_message_id);

-- The address book, one row per address: local books (collected addresses included)
-- and the read-only copies of contacts synced from an account's server (ADR-024; book
-- "google:<account uuid>" for My Contacts, "google-other:<account uuid>" for Other
-- contacts). `resource_name` is the synced person a row belongs to (NULL for local
-- books).
CREATE TABLE contact (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  book          TEXT NOT NULL DEFAULT 'collected',
  display_name  TEXT,
  email         TEXT NOT NULL COLLATE NOCASE,
  use_count     INTEGER NOT NULL DEFAULT 0,
  last_used_at  INTEGER,
  vcard         TEXT,
  resource_name TEXT
);
CREATE UNIQUE INDEX contact_email ON contact (book, email);
CREATE INDEX contact_lookup ON contact (email COLLATE NOCASE);
CREATE INDEX contact_person_rows ON contact (book, resource_name)
  WHERE resource_name IS NOT NULL;

-- Where each synced book stands. Everything synced goes with the account.
CREATE TABLE contact_source (
  book              TEXT PRIMARY KEY NOT NULL,
  account_id        INTEGER NOT NULL REFERENCES account (id) ON DELETE CASCADE,
  -- "google" or "google-other" (the book name's prefix).
  kind              TEXT NOT NULL,
  -- The token of the next incremental sync; NULL: the next sync lists everything.
  sync_token        TEXT,
  last_full_sync_at INTEGER,
  last_sync_at      INTEGER,
  -- "ok", "missing_scope" (contacts need permission) or "error".
  state             TEXT NOT NULL DEFAULT 'ok'
                      CHECK (state IN ('ok', 'missing_scope', 'error')),
  error             TEXT
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
  -- The sync run that last saw the person (an increasing run number, the run's start
  -- in Unix milliseconds); a full sync removes the ones it didn't.
  sync_run      INTEGER NOT NULL DEFAULT 0,
  updated_at    INTEGER NOT NULL,
  PRIMARY KEY (book, resource_name)
) WITHOUT ROWID;

-- A person's rows go with the person (and so with the book and the account).
CREATE TRIGGER contact_person_deleted AFTER DELETE ON contact_person
BEGIN
  DELETE FROM contact WHERE book = OLD.book AND resource_name = OLD.resource_name;
END;

-- Photos fetched into the encrypted blob cache, by URL. `is_found` 0 remembers that
-- there is no photo behind the URL; entries are refreshed after about 7 days.
CREATE TABLE contact_photo (
  url        TEXT PRIMARY KEY NOT NULL,
  fetched_at INTEGER NOT NULL,
  is_found   INTEGER NOT NULL CHECK (is_found IN (0, 1))
) WITHOUT ROWID;

-- The domain index (PLAN-contacts §3b): every address's registrable domain (Public
-- Suffix List: `mail.eu.northwind.example` is `northwind.example`), so contacts and
-- mail can be connected by domain, and the state of each domain's favicon (fetched
-- from the domain itself).
--
-- - `domain`: one row per registrable domain (lowercase ASCII, punycode for IDN) seen
--   in mail, or whose favicon state is known. The counters are maintained by the
--   triggers below from `message_domain`, so they stay right when messages go
--   (expunge, purge, account removal cascade). `last_seen_at` is the newest message date
--   ever seen and is not lowered by deletes. Contact counts are not stored: the
--   `contact` table is small and they are derived when read. Whether a domain is a
--   free-mail provider is decided in code (`mazemail_mime::domain::is_personal`).
-- - `message_domain`: which domains a message involves, one row per message and
--   domain, written when the message is stored. `roles` is a bit set: 1 an address in
--   From, 2 one in To or Cc, 4 the message was sent by you (From is one of your
--   identities) to an address there. Your own identity addresses are not indexed.
-- - Favicons: `favicon_state` 0 unknown, 1 stored in the encrypted cache under the
--   random id `favicon_key`, 2 the domain has none (remembered until it is due again);
--   `favicon_checked_at` is when it was last fetched.
CREATE TABLE domain (
  id                 INTEGER PRIMARY KEY,
  name               TEXT NOT NULL UNIQUE,
  message_count      INTEGER NOT NULL DEFAULT 0 CHECK (message_count >= 0),
  from_count         INTEGER NOT NULL DEFAULT 0 CHECK (from_count >= 0),
  sent_count         INTEGER NOT NULL DEFAULT 0 CHECK (sent_count >= 0),
  last_seen_at       INTEGER,
  favicon_state      INTEGER NOT NULL DEFAULT 0 CHECK (favicon_state IN (0, 1, 2)),
  favicon_checked_at INTEGER,
  favicon_key        BLOB
);

CREATE TABLE message_domain (
  domain_id  INTEGER NOT NULL REFERENCES domain (id) ON DELETE CASCADE,
  message_id INTEGER NOT NULL REFERENCES message (id) ON DELETE CASCADE,
  roles      INTEGER NOT NULL CHECK (roles > 0),
  PRIMARY KEY (domain_id, message_id)
) WITHOUT ROWID;
CREATE INDEX message_domain_message ON message_domain (message_id);

-- Filters (CONCEPT §14). `account_id` NULL means every account.
CREATE TABLE rule (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  account_id     INTEGER REFERENCES account (id) ON DELETE CASCADE,
  name           TEXT NOT NULL,
  ordinal        INTEGER NOT NULL DEFAULT 0,
  is_enabled     INTEGER NOT NULL DEFAULT 1,
  trigger_kind   TEXT NOT NULL
                   CHECK (trigger_kind IN ('arrival_before_junk', 'arrival', 'manual', 'after_send', 'periodic')),
  condition_json TEXT NOT NULL,
  actions_json   TEXT NOT NULL,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL
);
CREATE INDEX rule_account ON rule (account_id, ordinal);

-- Thunderbird virtual folders. `folders_json` NULL means all folders.
CREATE TABLE saved_search (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  name         TEXT NOT NULL,
  query        TEXT NOT NULL,
  folders_json TEXT,
  ordinal      INTEGER NOT NULL DEFAULT 0,
  created_at   INTEGER NOT NULL
);

-- Only the BLAKE3 hash of a token is stored (PLAN §4.11).
CREATE TABLE api_token (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  hash         BLOB NOT NULL UNIQUE,
  profile      TEXT NOT NULL,
  scope_json   TEXT NOT NULL DEFAULT '{}',
  expires_at   INTEGER,
  label        TEXT,
  created_at   INTEGER NOT NULL,
  last_used_at INTEGER,
  revoked_at   INTEGER
);

CREATE TABLE proposal (
  id           INTEGER PRIMARY KEY AUTOINCREMENT,
  api_token_id INTEGER REFERENCES api_token (id) ON DELETE SET NULL,
  title        TEXT NOT NULL,
  state        TEXT NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'approved', 'rejected', 'partial', 'applied')),
  created_at   INTEGER NOT NULL,
  decided_at   INTEGER
);
CREATE INDEX proposal_state ON proposal (state, created_at);
CREATE INDEX proposal_api_token ON proposal (api_token_id) WHERE api_token_id IS NOT NULL;

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
  occurred_at INTEGER NOT NULL,
  origin      TEXT NOT NULL,
  action      TEXT NOT NULL,
  detail_json TEXT
);
CREATE INDEX audit_log_occurred ON audit_log (occurred_at);

-- Local drafts, what the composer autosaves while you write (PLAN §4.5): Markdown body,
-- recipients as JSON arrays of `{"name": …, "email": …}`, attachments as file
-- references (their content goes into the outbox blob when the message is sent).
-- `name` is the draft's random two-word name (`brave-otter`) that titles its tab
-- (ADR-018); `support_maze` whether "Sent with Maze (https://mazemail.dev)" is added
-- below the message (the composer's checkbox, defaulting to `compose.support_maze`).
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
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL,
  name             TEXT NOT NULL DEFAULT '',
  support_maze     INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX draft_identity ON draft (identity_id) WHERE identity_id IS NOT NULL;
CREATE INDEX draft_updated ON draft (updated_at DESC);

-- Senders whose messages may load remote content (images, fonts, CSS backgrounds)
-- without asking, added with "Always for this sender" in the message view
-- (reader.remote_content = "ask"). Addresses are stored lowercase.
CREATE TABLE remote_content_sender (
  email    TEXT PRIMARY KEY NOT NULL,
  added_at INTEGER NOT NULL
) WITHOUT ROWID;

-- The personal spelling dictionary, words added with "Add to Dictionary" in the
-- composer. One list for every language (words are often names); kept in the
-- encrypted database because they are.
CREATE TABLE spelling_word (
  word     TEXT PRIMARY KEY NOT NULL,
  added_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE agent_conversation (
  id                 INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_id           TEXT NOT NULL,
  title              TEXT,
  mail_thread_id     INTEGER REFERENCES thread (id) ON DELETE SET NULL,
  acp_session_id     TEXT,
  transcript_blob_id BLOB REFERENCES blob (id),
  created_at         INTEGER NOT NULL,
  updated_at         INTEGER NOT NULL
);
CREATE INDEX agent_conversation_updated ON agent_conversation (updated_at DESC);
CREATE INDEX agent_conversation_mail ON agent_conversation (mail_thread_id)
  WHERE mail_thread_id IS NOT NULL;
CREATE INDEX agent_conversation_blob ON agent_conversation (transcript_blob_id)
  WHERE transcript_blob_id IS NOT NULL;

-- Message dates and attachment flags on their locations (see `message_location`; the
-- date never changes after insert, the attachment flag does when the body arrives).
CREATE TRIGGER message_date_update AFTER UPDATE OF date ON message
  WHEN NEW.date IS NOT OLD.date
BEGIN
  UPDATE message_location SET date = NEW.date WHERE message_id = NEW.id;
END;

CREATE TRIGGER message_attachment_update AFTER UPDATE OF has_attachments ON message
  WHEN NEW.has_attachments IS NOT OLD.has_attachments
BEGIN
  UPDATE message_location SET has_attachments = NEW.has_attachments WHERE message_id = NEW.id;
END;

-- Domain counters (see `domain`).
CREATE TRIGGER message_domain_insert AFTER INSERT ON message_domain
BEGIN
  UPDATE domain SET
    message_count = message_count + 1,
    from_count = from_count + (NEW.roles & 1 <> 0),
    sent_count = sent_count + (NEW.roles & 4 <> 0),
    last_seen_at = max(COALESCE(last_seen_at, 0),
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

-- Blob reference counts.
CREATE TRIGGER message_blob_insert AFTER INSERT ON message WHEN NEW.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER message_blob_update AFTER UPDATE OF blob_id ON message
  WHEN NEW.blob_id IS NOT OLD.blob_id
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.blob_id;
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER message_blob_delete AFTER DELETE ON message WHEN OLD.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.blob_id;
END;

CREATE TRIGGER outbox_blob_insert AFTER INSERT ON outbox WHEN NEW.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER outbox_blob_update AFTER UPDATE OF blob_id ON outbox
  WHEN NEW.blob_id IS NOT OLD.blob_id
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.blob_id;
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER outbox_blob_delete AFTER DELETE ON outbox WHEN OLD.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.blob_id;
END;

CREATE TRIGGER pending_op_blob_insert AFTER INSERT ON pending_op WHEN NEW.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER pending_op_blob_update AFTER UPDATE OF blob_id ON pending_op
  WHEN NEW.blob_id IS NOT OLD.blob_id
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.blob_id;
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER pending_op_blob_delete AFTER DELETE ON pending_op WHEN OLD.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.blob_id;
END;

CREATE TRIGGER agent_conversation_blob_insert AFTER INSERT ON agent_conversation
  WHEN NEW.transcript_blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.transcript_blob_id;
END;
CREATE TRIGGER agent_conversation_blob_update AFTER UPDATE OF transcript_blob_id ON agent_conversation
  WHEN NEW.transcript_blob_id IS NOT OLD.transcript_blob_id
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.transcript_blob_id;
  UPDATE blob SET ref_count = ref_count + 1 WHERE id = NEW.transcript_blob_id;
END;
CREATE TRIGGER agent_conversation_blob_delete AFTER DELETE ON agent_conversation
  WHEN OLD.transcript_blob_id IS NOT NULL
BEGIN
  UPDATE blob SET ref_count = ref_count - 1 WHERE id = OLD.transcript_blob_id;
END;

-- Thunderbird's default tags, so `$label1`…`$label5` from the server show up as tags.
INSERT INTO tag (keyword, name, color, ordinal) VALUES
  ('$label1', 'Important', '#FF0000', 1),
  ('$label2', 'Work', '#FF9900', 2),
  ('$label3', 'Personal', '#009900', 3),
  ('$label4', 'To Do', '#3333FF', 4),
  ('$label5', 'Later', '#993399', 5);

-- Maze store, schema v10: IMAP only (ADR-019).
--
-- 1. Maze no longer has local accounts ("Local Folders"). Local accounts without a
--    single message are removed (their folders, identities, rules and ops go with them
--    through the foreign keys). Local accounts that still hold mail are kept: Maze
--    shows them read-only, says at every start that they are no longer supported and
--    how to export them (`mazemail export`), and a later version removes them. User
--    mail is never deleted here.
-- 2. `mazemail import mbox|maildir` now uploads into an IMAP folder through queued
--    `append` ops. Such an op is the only reference to its message until the server
--    has it, so `pending_op.blob_id` holds the blob, counted like every other blob
--    reference (triggers below), and garbage collection leaves it alone.

DELETE FROM account
WHERE kind = 'local'
  AND NOT EXISTS (SELECT 1 FROM message WHERE message.account_id = account.id);

ALTER TABLE pending_op ADD COLUMN blob_id BLOB REFERENCES blob (id);
CREATE INDEX pending_op_blob ON pending_op (blob_id) WHERE blob_id IS NOT NULL;

CREATE TRIGGER pending_op_blob_insert AFTER INSERT ON pending_op WHEN NEW.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER pending_op_blob_update AFTER UPDATE OF blob_id ON pending_op
  WHEN NEW.blob_id IS NOT OLD.blob_id
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.blob_id;
  UPDATE blob SET refs = refs + 1 WHERE id = NEW.blob_id;
END;
CREATE TRIGGER pending_op_blob_delete AFTER DELETE ON pending_op WHEN OLD.blob_id IS NOT NULL
BEGIN
  UPDATE blob SET refs = refs - 1 WHERE id = OLD.blob_id;
END;

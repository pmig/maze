-- Maze store, schema v3: message lists for the unthreaded view (PLAN §4.3).
--
-- With threads off, the list shows one row per message, ordered by date, with the same
-- keyset pagination as thread lists. Each location gets the message's date and
-- attachment flag, so a folder's messages can be read in date order (and filtered by
-- attachment) straight from an index. The writer fills both when it adds a location;
-- triggers keep them right when the message changes (the attachment flag does when
-- the body arrives; the date never does after insert).

ALTER TABLE message_location ADD COLUMN date INTEGER NOT NULL DEFAULT 0;
ALTER TABLE message_location ADD COLUMN attachment INTEGER NOT NULL DEFAULT 0;

UPDATE message_location
  SET (date, attachment) = (SELECT m.date, m.has_attachments FROM message m
                            WHERE m.id = message_location.message_id);

-- Folder message lists. The partial indexes make the unread, starred and attachment
-- quick filters range scans (SQLite still evaluates the WHERE term).
CREATE INDEX location_list ON message_location (folder_id, date DESC, message_id DESC);
CREATE INDEX location_list_unread ON message_location (folder_id, date DESC, message_id DESC)
  WHERE flags & 1 = 0;
CREATE INDEX location_list_flagged ON message_location (folder_id, date DESC, message_id DESC)
  WHERE flags & 4 <> 0;
CREATE INDEX location_list_attachments
  ON message_location (folder_id, date DESC, message_id DESC)
  WHERE attachment = 1;

-- Account-scope message lists.
CREATE INDEX message_list ON message (account_id, date DESC, id DESC) WHERE detached_at IS NULL;

CREATE TRIGGER message_date_update AFTER UPDATE OF date ON message
  WHEN NEW.date IS NOT OLD.date
BEGIN
  UPDATE message_location SET date = NEW.date WHERE message_id = NEW.id;
END;

CREATE TRIGGER message_attachment_update AFTER UPDATE OF has_attachments ON message
  WHEN NEW.has_attachments IS NOT OLD.has_attachments
BEGIN
  UPDATE message_location SET attachment = NEW.has_attachments WHERE message_id = NEW.id;
END;

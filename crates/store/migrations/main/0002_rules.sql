-- The rules engine (ADR-029): where it stopped in each folder it watches for new
-- mail, and the messages it has looked at, so each is processed once.

-- The highest UID the arrival rules have seen in a folder (the Inbox, or Gmail's All
-- Mail), per UIDVALIDITY. Mail at or below it is never new to the rules.
CREATE TABLE rule_cursor (
  folder_id   INTEGER PRIMARY KEY REFERENCES folder (id) ON DELETE CASCADE,
  uidvalidity INTEGER NOT NULL,
  last_uid    INTEGER NOT NULL
);

-- Messages the arrival rules have processed: a message moved back into the Inbox (it
-- gets a new UID there) isn't processed again.
CREATE TABLE rule_message (
  message_id   INTEGER PRIMARY KEY REFERENCES message (id) ON DELETE CASCADE,
  processed_at INTEGER NOT NULL
);

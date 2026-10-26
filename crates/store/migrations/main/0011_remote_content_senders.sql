-- Maze store, schema v11: senders whose messages may load remote content (images,
-- fonts, CSS backgrounds) without asking, added with "Always for this sender" in the
-- message view (reader.remote_content = "ask"). Addresses are stored lowercase.

CREATE TABLE remote_content_sender (
    email TEXT PRIMARY KEY NOT NULL,
    added INTEGER NOT NULL
) WITHOUT ROWID;

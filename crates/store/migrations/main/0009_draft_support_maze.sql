-- Maze store, schema v9: whether a draft adds "Sent with Maze (https://mazemail.dev)"
-- below the message when it is sent (the composer's checkbox, defaulting to the
-- `compose.support_maze` setting). Drafts from before keep the default, on.

ALTER TABLE draft ADD COLUMN support_maze INTEGER NOT NULL DEFAULT 1;

-- Maze store, schema v6: a draft's random two-word name (`brave-otter`), which titles
-- its tab in the main window (ADR-018). Drafts from before get an empty name; Maze
-- names them when they are opened.

ALTER TABLE draft ADD COLUMN name TEXT NOT NULL DEFAULT '';

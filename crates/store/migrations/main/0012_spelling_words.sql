-- Maze store, schema v12: the personal spelling dictionary, words added with "Add to
-- Dictionary" in the composer. One list for every language (words are often names);
-- kept in the encrypted database because they are.

CREATE TABLE spelling_word (
    word TEXT PRIMARY KEY NOT NULL,
    added INTEGER NOT NULL
) WITHOUT ROWID;

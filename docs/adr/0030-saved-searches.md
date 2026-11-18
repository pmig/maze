# ADR-030: Saved searches in the folder pane

- **Status:** Accepted
- **Date:** 2026-11-18

## Context

Thunderbird's "Saved Search" (a virtual folder) is a named query that the folder pane
lists like a folder. Maze imports them from `virtualFolders.dat` (the Thunderbird
import translates the terms into Maze's query language) into the store's
`saved_search` table, but nothing showed them. Search across all mail already exists:
the title bar's search field (`cmd-shift-f`) shows its results in a mail view, and
`mazemail search` prints them.

Questions: where saved searches appear, how their results stay current, where they
are stored and how they relate to favorites.

## Decision

**They appear in a "Saved Searches" section** of the folder pane, between Unified
Folders and Accounts, shown only when there is at least one. The pane always shows its
sections (Favorites, Unified Folders, Accounts); `folder_pane.modes` is not wired to
the pane yet, so a `saved` mode would be a setting that does nothing. A saved search is
a `FolderKey::SavedSearch(id)` in the UI: selecting it, Open in New Tab, Go to Folder
(`cmd-k`), tab restore (the layout keeps the id) and the pane's selection work like for
a folder. Its menu offers Edit Saved Search… (name and query in one dialog) and Delete
Saved Search… (asks first). "Save Search…" (the search results' header, `cmd-alt-s`,
the palette) saves the shown search under a name and opens it.

**Results are a live search.** Nothing about the results is stored. Opening a saved
search runs its query over every account through the same search as the search field
(`CoreHandle::search_saved`); the Quick Filter and its toggles narrow it (`(query)
filter`). The list re-runs it whenever threads change, like any open search, so new,
read, moved and deleted mail shows up at once. Deleted and junk mail stays out unless
the query asks for it, as in every search. There are no unread counts yet: each count
would be one more search per change batch.

**They stay in the store**, in the existing `saved_search` table, next to tags and
rules: the import writes them there, they are per data directory rather than a
preference, and the CLI and the UI change them through `mazemail_core`. No schema
change was needed. Names are unique ignoring case, so the CLI can name one instead of
giving its id (`s_…`). `folders_json` (a Thunderbird scope) is kept but not applied:
every saved search searches all folders.

**Favorites stay folders of an account.** A saved search can't be a favorite; its
section already pins it near the top. `folder_pane.favorites` is unchanged.

**CLI:** `mazemail saved-search list|add|rename|edit|delete`, and `mazemail search
--saved <name|id> [query]` runs one (the query narrows it). Listing is `read`; adding,
renaming and editing are `draft` (they change what Maze shows, never mail); deleting
asks (`ask`), because it loses the user's query. The saved searches are global, not per
account, so every token sees their names and queries; `search --saved` still only
returns mail in the token's scope.

## Consequences

- Saved searches behave like search results: as fast as the search, limited like it
  (the first 10,000 threads), with no per-folder context in the rows.
- Imported Thunderbird searches whose scope was a few folders now search everything.
  Applying `folders_json` (and a scope picker in the dialog) is a later step.
- Unread counts, reordering (the `ordinal` column exists) and a `saved` folder pane mode
  can follow without a schema change.

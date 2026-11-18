//! Saved searches (ADR-030): a named query that the folder pane shows like a folder,
//! Thunderbird's "Saved Search" (virtual folder).
//!
//! They live in the store's `saved_search` table, next to tags and rules: the
//! Thunderbird import fills it, the UI and `mazemail saved-search` change it. Nothing
//! about the results is stored. Opening a saved search runs its query through
//! [`CoreHandle::search`] over every account, so the results follow the mail as it
//! changes, like any search.
//!
//! Names are unique (ignoring case), so the CLI can name one instead of giving its id.
//! The query must parse; it is kept as typed. `folders_json` (a Thunderbird scope) is
//! kept but not applied yet: every saved search searches all folders.

use mazemail_types::{PublicId as _, SavedSearchId};
use search::query::parse;
use store::{Queries as _, SavedSearch};

use crate::global_search::{SearchOpts, SearchResults};
use crate::{CoreHandle, Error, Result};

/// Longest name, in characters.
pub const MAX_NAME_CHARS: usize = 200;

/// A change to a saved search: what is `Some` is replaced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SavedSearchChange {
    /// A new name.
    pub name: Option<String>,
    /// A new query.
    pub query: Option<String>,
}

/// The name, trimmed, or why it can't be one.
fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::InvalidCommand("a saved search needs a name"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(Error::InvalidCommand(
            "the name of a saved search is too long",
        ));
    }
    Ok(name.to_owned())
}

/// The query, trimmed, or why it can't be one.
fn clean_query(query: &str) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::InvalidCommand("a saved search needs a query"));
    }
    parse(query)?;
    Ok(query.to_owned())
}

/// Whether another saved search than `except` is called `name` (ignoring case).
fn name_taken(searches: &[SavedSearch], name: &str, except: Option<SavedSearchId>) -> bool {
    searches
        .iter()
        .any(|s| Some(s.id) != except && s.name.to_lowercase() == name.to_lowercase())
}

const NAME_TAKEN: &str = "a saved search with that name exists already";

/// `query` narrowed by `extra` (the Quick Filter's text): both must match.
pub fn combined_query(query: &str, extra: Option<&str>) -> String {
    match extra.map(str::trim).filter(|e| !e.is_empty()) {
        Some(extra) => format!("({query}) {extra}"),
        None => query.to_owned(),
    }
}

impl CoreHandle {
    /// Every saved search, in folder pane order (ordinal, then creation).
    pub fn saved_searches(&self) -> Result<Vec<SavedSearch>> {
        self.ensure_open()?;
        Ok(self.store().reader()?.saved_searches()?)
    }

    /// One saved search.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if there is none with this id.
    pub fn saved_search(&self, id: SavedSearchId) -> Result<SavedSearch> {
        self.saved_searches()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| Error::not_found("saved search", id))
    }

    /// The saved search `name_or_id` names: its public id (`s_…`) or its name (ignoring
    /// case).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if none matches.
    pub fn find_saved_search(&self, name_or_id: &str) -> Result<SavedSearch> {
        let wanted = name_or_id.trim();
        let searches = self.saved_searches()?;
        if let Ok(id) = SavedSearchId::parse_public(wanted)
            && let Some(found) = searches.iter().find(|s| s.id == id)
        {
            return Ok(found.clone());
        }
        let lower = wanted.to_lowercase();
        searches
            .into_iter()
            .find(|s| s.name.to_lowercase() == lower)
            .ok_or(Error::NotFound {
                entity: "saved search",
                id: "with that name or id".to_owned(),
            })
    }

    /// Saves `query` as a saved search called `name`, after the others.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidCommand`] for an empty or taken name or an empty query,
    /// [`Error::Query`] for a query that doesn't parse.
    pub async fn create_saved_search(&self, name: &str, query: &str) -> Result<SavedSearch> {
        self.ensure_open()?;
        let name = clean_name(name)?;
        let query = clean_query(query)?;
        let created = self
            .store()
            .write(move |w| {
                let searches = w.saved_searches()?;
                if name_taken(&searches, &name, None) {
                    return Ok(None);
                }
                let ordinal = searches.iter().map(|s| s.ordinal + 1).max().unwrap_or(0);
                w.create_saved_search(&name, &query, None, ordinal)
                    .map(Some)
            })
            .await?;
        let id = created.ok_or(Error::InvalidCommand(NAME_TAKEN))?;
        self.saved_search(id)
    }

    /// Renames a saved search or changes its query.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if it doesn't exist, else like
    /// [`CoreHandle::create_saved_search`].
    pub async fn update_saved_search(
        &self,
        id: SavedSearchId,
        change: SavedSearchChange,
    ) -> Result<SavedSearch> {
        self.ensure_open()?;
        let name = change.name.as_deref().map(clean_name).transpose()?;
        let query = change.query.as_deref().map(clean_query).transpose()?;
        let updated = self
            .store()
            .write(move |w| {
                let searches = w.saved_searches()?;
                let Some(current) = searches.iter().find(|s| s.id == id) else {
                    return Ok(Err(Error::not_found("saved search", id)));
                };
                let name = name.unwrap_or_else(|| current.name.clone());
                if name_taken(&searches, &name, Some(id)) {
                    return Ok(Err(Error::InvalidCommand(NAME_TAKEN)));
                }
                let query = query.unwrap_or_else(|| current.query.clone());
                w.update_saved_search(
                    id,
                    &name,
                    &query,
                    current.folders.as_deref(),
                    current.ordinal,
                )?;
                Ok(Ok(()))
            })
            .await?;
        updated?;
        self.saved_search(id)
    }

    /// Deletes a saved search (the mail it finds stays where it is).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if it doesn't exist.
    pub async fn delete_saved_search(&self, id: SavedSearchId) -> Result<()> {
        self.ensure_open()?;
        self.saved_search(id)?;
        self.store()
            .write(move |w| w.delete_saved_search(id))
            .await?;
        Ok(())
    }

    /// Runs a saved search, narrowed by `extra` (the Quick Filter's text) if given.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if it doesn't exist, [`Error::Query`] if its query no longer
    /// parses (an imported one).
    pub async fn search_saved(
        &self,
        id: SavedSearchId,
        extra: Option<&str>,
        opts: SearchOpts,
    ) -> Result<SearchResults> {
        let saved = self.saved_search(id)?;
        let query = combined_query(&saved.query, extra);
        self.search(&query, opts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_queries_are_trimmed_and_checked() {
        assert_eq!(clean_name("  Invoices ").unwrap(), "Invoices");
        assert!(matches!(clean_name("  "), Err(Error::InvalidCommand(_))));
        assert!(matches!(
            clean_name(&"x".repeat(MAX_NAME_CHARS + 1)),
            Err(Error::InvalidCommand(_))
        ));
        assert_eq!(
            clean_query(" from:anna is:unread ").unwrap(),
            "from:anna is:unread"
        );
        assert!(matches!(clean_query(""), Err(Error::InvalidCommand(_))));
        assert!(matches!(clean_query("from:(anna"), Err(Error::Query(_))));
    }

    #[test]
    fn the_quick_filter_narrows_the_query() {
        assert_eq!(combined_query("a OR b", None), "a OR b");
        assert_eq!(combined_query("a OR b", Some("  ")), "a OR b");
        assert_eq!(combined_query("a OR b", Some("c")), "(a OR b) c");
    }
}

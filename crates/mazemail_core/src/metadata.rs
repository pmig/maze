//! [`search::MetadataSource`] over the store: the mutable half of a search query
//! (folders, flags, tags, account), evaluated in SQLite (PLAN §4.4, "Mutable fields").
//!
//! The query's metadata part is a boolean tree; the store answers conjunctions
//! ([`store::MetadataFilter`]) over candidate ids or narrow scopes. Evaluation:
//!
//! - a term becomes one store filter, applied to the candidates
//!   ([`Queries::filter_message_ids`]) or, without candidates, turned into an id set
//!   ([`Queries::message_ids_in_scope`]) when the term is a scope (folder, role,
//!   account);
//! - `And` narrows the candidates term by term, `Or` unions, `Not` subtracts from the
//!   candidates (or from every message, which is computed once per search).
//!
//! Names resolve case-insensitively: `in:` against folder display and server names,
//! `tag:` against tag keywords and names, `account:` against account names. A name
//! that matches nothing matches no message.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use mazemail_types::{AccountId, Flags, FolderId, MessageId, TagId};
use search::plan::MetadataTerm;
use search::query::Mailbox;
use search::{MetadataFilter, MetadataSource};
use store::{Account, Folder, MessageScope, Queries, Reader, Tag};

/// Evaluates search metadata filters with one pooled reader.
pub(crate) struct StoreMetadata<'r> {
    reader: &'r Reader,
    accounts: Vec<Account>,
    folders: Vec<Folder>,
    tags: Vec<Tag>,
    universe: OnceLock<BTreeSet<MessageId>>,
}

impl std::fmt::Debug for StoreMetadata<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreMetadata")
            .field("accounts", &self.accounts.len())
            .field("folders", &self.folders.len())
            .finish_non_exhaustive()
    }
}

/// One term, resolved to store ids.
enum Resolved {
    Filter(store::MetadataFilter),
    /// A union of filters (a name that matched several folders' accounts or tags).
    AnyOf(Vec<store::MetadataFilter>),
    Nothing,
}

impl<'r> StoreMetadata<'r> {
    pub(crate) fn new(reader: &'r Reader) -> store::Result<Self> {
        Ok(Self {
            accounts: reader.accounts()?,
            folders: reader.folders(None)?,
            tags: reader.tags()?,
            universe: OnceLock::new(),
            reader,
        })
    }

    /// Every message the store knows (with a location).
    fn universe(&self) -> store::Result<&BTreeSet<MessageId>> {
        if let Some(all) = self.universe.get() {
            return Ok(all);
        }
        let mut all = BTreeSet::new();
        for account in &self.accounts {
            all.extend(
                self.reader
                    .message_ids_in_scope(&MessageScope::Account(account.id))?,
            );
        }
        Ok(self.universe.get_or_init(|| all))
    }

    fn folders_named(&self, name: &str) -> Vec<FolderId> {
        let lower = name.to_lowercase();
        self.folders
            .iter()
            .filter(|f| {
                f.display_name.to_lowercase() == lower
                    || f.remote_name
                        .as_deref()
                        .is_some_and(|r| r.to_lowercase() == lower)
            })
            .map(|f| f.id)
            .collect()
    }

    fn tags_named(&self, name: &str) -> Vec<TagId> {
        let lower = name.to_lowercase();
        self.tags
            .iter()
            .filter(|t| t.keyword == lower || t.name.to_lowercase() == lower)
            .map(|t| t.id)
            .collect()
    }

    fn accounts_named(&self, name: &str) -> Vec<AccountId> {
        let lower = name.to_lowercase();
        self.accounts
            .iter()
            .filter(|a| a.name.to_lowercase() == lower)
            .map(|a| a.id)
            .collect()
    }

    fn resolve(&self, term: &MetadataTerm) -> Resolved {
        let base = store::MetadataFilter::default;
        match term {
            MetadataTerm::In(Mailbox::Role(role)) => Resolved::Filter(store::MetadataFilter {
                in_role: Some(*role),
                ..base()
            }),
            MetadataTerm::In(Mailbox::Name(name)) => {
                let folders = self.folders_named(name);
                if folders.is_empty() {
                    Resolved::Nothing
                } else {
                    Resolved::Filter(store::MetadataFilter {
                        folders: Some(folders),
                        ..base()
                    })
                }
            }
            MetadataTerm::Tag(name) => {
                let tags = self.tags_named(name);
                match tags.len() {
                    0 => Resolved::Nothing,
                    _ => Resolved::AnyOf(
                        tags.into_iter()
                            .map(|tag| store::MetadataFilter {
                                tags: vec![tag],
                                ..base()
                            })
                            .collect(),
                    ),
                }
            }
            MetadataTerm::Flag { flag, set } => Resolved::Filter(if *set {
                store::MetadataFilter {
                    flags_set: *flag,
                    ..base()
                }
            } else {
                store::MetadataFilter {
                    flags_unset: *flag,
                    ..base()
                }
            }),
            MetadataTerm::Account(name) => {
                let accounts = self.accounts_named(name);
                match accounts.len() {
                    0 => Resolved::Nothing,
                    _ => Resolved::AnyOf(
                        accounts
                            .into_iter()
                            .map(|account| store::MetadataFilter {
                                account: Some(account),
                                ..base()
                            })
                            .collect(),
                    ),
                }
            }
        }
    }

    /// Ids matching one store filter, within `within` (or anywhere).
    fn apply(
        &self,
        filter: &store::MetadataFilter,
        within: Option<&BTreeSet<MessageId>>,
    ) -> store::Result<BTreeSet<MessageId>> {
        if let Some(within) = within {
            let candidates: Vec<MessageId> = within.iter().copied().collect();
            return Ok(self
                .reader
                .filter_message_ids(&candidates, filter)?
                .into_iter()
                .collect());
        }
        // Without candidates: a pure scope term becomes an id set directly.
        let is_scope_only = filter.flags_set == Flags::empty()
            && filter.flags_unset == Flags::empty()
            && filter.tags.is_empty();
        let scope = match (filter.folders.as_ref(), filter.account, filter.in_role) {
            (Some(folders), None, None) if is_scope_only => {
                Some(MessageScope::Folders(folders.clone()))
            }
            (None, Some(account), None) if is_scope_only => Some(MessageScope::Account(account)),
            (None, None, Some(role)) if is_scope_only => Some(MessageScope::Role(role)),
            _ => None,
        };
        match scope {
            Some(scope) => Ok(self
                .reader
                .message_ids_in_scope(&scope)?
                .into_iter()
                .collect()),
            None => {
                let all = self.universe()?;
                self.apply(filter, Some(all))
            }
        }
    }

    fn eval(
        &self,
        filter: &MetadataFilter,
        within: Option<&BTreeSet<MessageId>>,
    ) -> store::Result<BTreeSet<MessageId>> {
        match filter {
            MetadataFilter::And(items) => {
                let mut current: Option<BTreeSet<MessageId>> = within.cloned();
                for item in items {
                    let next = self.eval(item, current.as_ref())?;
                    if next.is_empty() {
                        return Ok(next);
                    }
                    current = Some(next);
                }
                match current {
                    Some(current) => Ok(current),
                    None => Ok(self.universe()?.clone()),
                }
            }
            MetadataFilter::Or(items) => {
                let mut union = BTreeSet::new();
                for item in items {
                    union.extend(self.eval(item, within)?);
                }
                Ok(union)
            }
            MetadataFilter::Not(inner) => {
                let base = match within {
                    Some(within) => within.clone(),
                    None => self.universe()?.clone(),
                };
                let excluded = self.eval(inner, Some(&base))?;
                Ok(base.difference(&excluded).copied().collect())
            }
            MetadataFilter::Term(term) => match self.resolve(term) {
                Resolved::Nothing => Ok(BTreeSet::new()),
                Resolved::Filter(filter) => self.apply(&filter, within),
                Resolved::AnyOf(filters) => {
                    let mut union = BTreeSet::new();
                    for filter in &filters {
                        union.extend(self.apply(filter, within)?);
                    }
                    Ok(union)
                }
            },
        }
    }
}

impl MetadataSource for StoreMetadata<'_> {
    type Error = store::Error;

    fn matching(
        &self,
        filter: &MetadataFilter,
        within: Option<&BTreeSet<MessageId>>,
    ) -> Result<BTreeSet<MessageId>, Self::Error> {
        self.eval(filter, within)
    }
}

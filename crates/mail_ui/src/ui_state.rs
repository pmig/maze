//! The message list's view, remembered across restarts: one for all of Maze, so every
//! folder, unified folder and search shows mail the same way (CONCEPT §8.2).
//!
//! # `ui_state.json`
//!
//! The app keeps it in the data directory (`~/Library/Application Support/mazemail/`
//! on macOS), next to the database, because folder ids belong to that database. It is
//! state Maze writes, not settings people edit (those are in `settings.json`), and it
//! holds no mail content: folder keys, view choices and column widths only.
//!
//! ```json
//! {
//!   "version": 2,
//!   "list": { "view": "table", "threaded": false, "sort": "oldest_first",
//!             "columns": ["star", "subject", "correspondents", "size", "date"],
//!             "widths": { "correspondents": 220.0 } },
//!   "move_targets": { "a2": [17, 4] }
//! }
//! ```
//!
//! `move_targets` are the folders Quick Move (`shift-m`) moved mail to last, per
//! account, newest first ([`RECENT_MOVE_TARGETS`] at most).
//!
//! What isn't in `list` follows the defaults from `settings.json`
//! (`thread_list.view`, `thread_list.threaded`, `thread_list.sort`), [`ListDefaults`]:
//! every field is optional, so `list` only records what was changed. Version 1 kept
//! a `folders` map with an entry per folder; it is ignored. Unknown fields and an
//! unreadable file are ignored (the file is rewritten on the next change); writes go
//! to a temporary file that is renamed into place.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use mazemail_types::{AccountId, FolderId};

use crate::source::ListSort;

/// The file name in the data directory.
pub const UI_STATE_FILE: &str = "ui_state.json";

/// Recent Quick Move targets remembered per account.
pub const RECENT_MOVE_TARGETS: usize = 5;

/// How the list draws its rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListMode {
    /// Two-line cards: sender and date, subject and markers.
    #[default]
    Cards,
    /// Thunderbird's table with columns.
    Table,
}

/// A column of the table view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnId {
    /// The star (`\Flagged`).
    Star,
    /// Unread dot.
    Unread,
    /// Paperclip.
    Attachment,
    /// Subject, with the thread twisty and the reply tree.
    Subject,
    /// Sender of received mail, recipients of mail you sent (Thunderbird).
    Correspondents,
    /// Always the sender.
    Sender,
    /// Always the recipients.
    Recipients,
    /// Tag names.
    Tags,
    /// The account.
    Account,
    /// Size.
    Size,
    /// Date (click the header to sort).
    Date,
}

impl ColumnId {
    /// Every column in display order (columns aren't reordered; the picker only turns
    /// them on and off).
    pub const ALL: [Self; 11] = [
        Self::Star,
        Self::Unread,
        Self::Attachment,
        Self::Subject,
        Self::Correspondents,
        Self::Sender,
        Self::Recipients,
        Self::Tags,
        Self::Account,
        Self::Size,
        Self::Date,
    ];

    /// The default set: Star · Subject · Correspondents · Date (CONCEPT §8.2).
    pub const DEFAULT: [Self; 4] = [Self::Star, Self::Subject, Self::Correspondents, Self::Date];

    /// The name in the header and the column picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::Star => "Star",
            Self::Unread => "Unread",
            Self::Attachment => "Attachment",
            Self::Subject => "Subject",
            Self::Correspondents => "Correspondents",
            Self::Sender => "Sender",
            Self::Recipients => "Recipients",
            Self::Tags => "Tags",
            Self::Account => "Account",
            Self::Size => "Size",
            Self::Date => "Date",
        }
    }

    /// The width a column starts with.
    pub fn default_width(self) -> f32 {
        match self {
            Self::Star | Self::Unread | Self::Attachment => 26.,
            Self::Subject => 280.,
            Self::Correspondents => 150.,
            Self::Sender | Self::Recipients => 150.,
            Self::Tags => 90.,
            Self::Account => 120.,
            Self::Size => 64.,
            Self::Date => 80.,
        }
    }

    /// Narrow icon columns keep their width; the Subject takes the rest.
    pub fn resizable(self) -> bool {
        !matches!(
            self,
            Self::Star | Self::Unread | Self::Attachment | Self::Subject
        )
    }

    /// The subject can't be hidden.
    pub fn hideable(self) -> bool {
        self != Self::Subject
    }

    /// The JSON name.
    pub fn key(self) -> &'static str {
        match self {
            Self::Star => "star",
            Self::Unread => "unread",
            Self::Attachment => "attachment",
            Self::Subject => "subject",
            Self::Correspondents => "correspondents",
            Self::Sender => "sender",
            Self::Recipients => "recipients",
            Self::Tags => "tags",
            Self::Account => "account",
            Self::Size => "size",
            Self::Date => "date",
        }
    }
}

/// The defaults for what the list view hasn't changed, from `settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListDefaults {
    /// `thread_list.view`.
    pub mode: ListMode,
    /// `thread_list.threaded`.
    pub threaded: bool,
    /// `thread_list.sort` (date orders only).
    pub sort: ListSort,
}

impl Default for ListDefaults {
    fn default() -> Self {
        Self {
            mode: ListMode::Cards,
            threaded: true,
            sort: ListSort::NewestFirst,
        }
    }
}

/// What is remembered of the list view. `None` follows the defaults.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListState {
    /// Cards or table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view: Option<ListMode>,
    /// Threads on or off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threaded: Option<bool>,
    /// Date order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<ListSort>,
    /// Table columns that are on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<ColumnId>>,
    /// Column widths the user set, in logical pixels.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub widths: BTreeMap<ColumnId, f32>,
}

/// The effective list settings: the remembered state over the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct ListPrefs {
    /// Cards or table.
    pub mode: ListMode,
    /// Threads on or off.
    pub threaded: bool,
    /// Date order.
    pub sort: ListSort,
    /// Table columns that are on, in display order.
    pub columns: Vec<ColumnId>,
    /// Column widths that differ from [`ColumnId::default_width`].
    pub widths: BTreeMap<ColumnId, f32>,
}

impl ListPrefs {
    /// `state` over `defaults`.
    pub fn resolve(state: Option<&ListState>, defaults: &ListDefaults) -> Self {
        let state = state.cloned().unwrap_or_default();
        let mut columns: Vec<ColumnId> = state
            .columns
            .unwrap_or_else(|| ColumnId::DEFAULT.to_vec())
            .into_iter()
            .collect();
        if !columns.contains(&ColumnId::Subject) {
            columns.push(ColumnId::Subject);
        }
        columns.sort_unstable();
        columns.dedup();
        Self {
            mode: state.view.unwrap_or(defaults.mode),
            threaded: state.threaded.unwrap_or(defaults.threaded),
            sort: state.sort.unwrap_or(defaults.sort),
            columns,
            widths: state.widths,
        }
    }

    /// The width of a column.
    pub fn width(&self, column: ColumnId) -> f32 {
        self.widths
            .get(&column)
            .copied()
            .unwrap_or_else(|| column.default_width())
    }

    /// Turns an optional column on or off (the subject stays).
    pub fn toggle_column(&mut self, column: ColumnId) {
        if !column.hideable() {
            return;
        }
        if let Some(ix) = self.columns.iter().position(|c| *c == column) {
            self.columns.remove(ix);
        } else {
            self.columns.push(column);
            self.columns.sort_unstable();
        }
    }

    /// What to remember: only what differs from the defaults.
    pub fn to_state(&self, defaults: &ListDefaults) -> ListState {
        ListState {
            view: (self.mode != defaults.mode).then_some(self.mode),
            threaded: (self.threaded != defaults.threaded).then_some(self.threaded),
            sort: (self.sort != defaults.sort).then_some(self.sort),
            columns: (self.columns != ColumnId::DEFAULT).then(|| self.columns.clone()),
            widths: self.widths.clone(),
        }
    }
}

/// The contents of `ui_state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// Format version, 2.
    pub version: u32,
    /// The list view, the same in every folder.
    pub list: ListState,
    /// Quick Move targets per account (keyed by its public id, `a2`), newest first.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub move_targets: BTreeMap<String, Vec<FolderId>>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            version: 2,
            list: ListState::default(),
            move_targets: BTreeMap::new(),
        }
    }
}

/// Loads and saves [`UiState`]; in memory only without a path (tests, demos).
#[derive(Debug, Clone)]
pub struct UiStateStore {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    path: Option<PathBuf>,
    state: UiState,
    defaults: ListDefaults,
}

impl UiStateStore {
    /// State kept in memory only.
    pub fn in_memory(defaults: ListDefaults) -> Self {
        Self::with(None, UiState::default(), defaults)
    }

    /// Reads `path` (a missing or unreadable file is an empty state).
    pub fn load(path: impl Into<PathBuf>, defaults: ListDefaults) -> Self {
        let path = path.into();
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, "ignoring an unreadable ui_state.json");
                UiState::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => UiState::default(),
            Err(error) => {
                tracing::warn!(%error, "cannot read ui_state.json");
                UiState::default()
            }
        };
        Self::with(Some(path), state, defaults)
    }

    fn with(path: Option<PathBuf>, state: UiState, defaults: ListDefaults) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                state,
                defaults,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The defaults for what the list view hasn't changed.
    pub fn defaults(&self) -> ListDefaults {
        self.lock().defaults
    }

    /// The effective list settings.
    pub fn prefs(&self) -> ListPrefs {
        let inner = self.lock();
        ListPrefs::resolve(Some(&inner.state.list), &inner.defaults)
    }

    /// Remembers the list settings and writes the file (if there is one).
    pub fn set_prefs(&self, prefs: &ListPrefs) {
        self.change(|inner| {
            inner.state.list = prefs.to_state(&inner.defaults);
        });
    }

    /// The folders Quick Move moved `account`'s mail to last, newest first.
    pub fn move_targets(&self, account: AccountId) -> Vec<FolderId> {
        self.lock()
            .state
            .move_targets
            .get(&account.to_string())
            .cloned()
            .unwrap_or_default()
    }

    /// Puts `folder` first among `account`'s recent move targets and writes the file.
    pub fn remember_move_target(&self, account: AccountId, folder: FolderId) {
        self.change(|inner| {
            let targets = inner
                .state
                .move_targets
                .entry(account.to_string())
                .or_default();
            targets.retain(|f| *f != folder);
            targets.insert(0, folder);
            targets.truncate(RECENT_MOVE_TARGETS);
        });
    }

    /// Changes the state and writes the file (if there is one).
    fn change(&self, edit: impl FnOnce(&mut Inner)) {
        let snapshot = {
            let mut inner = self.lock();
            edit(&mut inner);
            inner.path.clone().map(|path| (path, inner.state.clone()))
        };
        if let Some((path, state)) = snapshot
            && let Err(error) = write_atomically(&path, &state)
        {
            tracing::warn!(%error, "cannot write ui_state.json");
        }
    }

    /// The current state (for tests and diagnostics).
    pub fn state(&self) -> UiState {
        self.lock().state.clone()
    }
}

fn write_atomically(path: &std::path::Path, state: &UiState) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_state_the_defaults_apply() {
        let store = UiStateStore::in_memory(ListDefaults {
            mode: ListMode::Table,
            threaded: false,
            sort: ListSort::NewestFirst,
        });
        let prefs = store.prefs();
        assert_eq!(prefs.mode, ListMode::Table);
        assert!(!prefs.threaded);
        assert_eq!(prefs.columns, ColumnId::DEFAULT);
        assert_eq!(prefs.width(ColumnId::Date), 80.);
    }

    #[test]
    fn remembers_the_view_and_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        let defaults = ListDefaults::default();
        let store = UiStateStore::load(&path, defaults);
        let mut prefs = store.prefs();
        prefs.mode = ListMode::Table;
        prefs.threaded = false;
        prefs.sort = ListSort::OldestFirst;
        prefs.toggle_column(ColumnId::Size);
        prefs.toggle_column(ColumnId::Star);
        prefs.toggle_column(ColumnId::Subject);
        prefs.widths.insert(ColumnId::Correspondents, 220.);
        store.set_prefs(&prefs);
        assert_eq!(
            prefs.columns,
            vec![
                ColumnId::Subject,
                ColumnId::Correspondents,
                ColumnId::Size,
                ColumnId::Date
            ],
            "columns keep their order; the subject can't be hidden"
        );

        let reopened = UiStateStore::load(&path, defaults);
        assert_eq!(reopened.prefs(), prefs);
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"list\""), "{json}");
        assert!(json.contains("\"oldest_first\""), "{json}");

        // Back to the defaults: nothing is recorded.
        store.set_prefs(&ListPrefs::resolve(None, &defaults));
        assert_eq!(store.state().list, ListState::default());
    }

    #[test]
    fn per_folder_state_from_version_1_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        std::fs::write(
            &path,
            r#"{ "version": 1, "folders": { "folder:12": { "view": "table" } } }"#,
        )
        .unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(store.prefs().mode, ListMode::Cards);
    }

    #[test]
    fn remembers_recent_move_targets_per_account() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        let store = UiStateStore::load(&path, ListDefaults::default());
        let (work, home) = (AccountId(1), AccountId(2));
        assert!(store.move_targets(work).is_empty());
        for folder in [3, 4, 3, 5, 6, 7, 8, 9] {
            store.remember_move_target(work, FolderId(folder));
        }
        store.remember_move_target(home, FolderId(20));
        let expected: Vec<_> = [9, 8, 7, 6, 5].map(FolderId).into();
        assert_eq!(
            store.move_targets(work),
            expected,
            "newest first, no repeats"
        );
        assert_eq!(store.move_targets(home), [FolderId(20)]);

        let reopened = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(reopened.move_targets(work), expected);
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"move_targets\""), "{json}");
    }

    #[test]
    fn a_broken_file_is_an_empty_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        std::fs::write(&path, b"{ not json").unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(store.state(), UiState::default());
        // Unknown fields and columns are fine; the subject is always there.
        std::fs::write(
            &path,
            br#"{"version":2,"later":true,"list":{"columns":["date"],"x":1}}"#,
        )
        .unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(
            store.prefs().columns,
            vec![ColumnId::Subject, ColumnId::Date]
        );
    }
}

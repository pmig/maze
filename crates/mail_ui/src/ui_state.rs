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
//!   "version": 5,
//!   "list": { "view": "table", "threaded": false, "sort": "subject_asc",
//!             "columns": ["star", "unread", "subject", "correspondents", "size", "date"],
//!             "widths": { "correspondents": 220.0 } },
//!   "move_targets": { "a2": [17, 4] },
//!   "address_book": { "book": "collected", "sort": "last_used" },
//!   "layout": {
//!     "root": { "split": { "axis": "horizontal", "flexes": [1.0, 1.0], "members": [
//!       { "pane": { "active": 0, "items": [
//!         { "kind": "mail", "data": { "type": "unified", "role": "inbox" } },
//!         { "kind": "message", "data": { "message": 4711 } } ] } },
//!       { "pane": { "active": 0, "items": [
//!         { "kind": "compose", "data": { "draft": 17 }, "title": "brave-otter" } ] } }
//!     ] } },
//!     "active_pane": 1
//!   }
//! }
//! ```
//!
//! `move_targets` are the folders Quick Move (`shift-m`) moved mail to last, per
//! account, newest first ([`RECENT_MOVE_TARGETS`] at most).
//!
//! `layout` (version 3) is the center's panes and tabs (ADR-018): splits with their
//! sizes, and per pane its tabs and the active one. Tabs record ids only (folders,
//! messages, drafts) plus a draft's random two-word name, never subjects. Tabs whose
//! message or draft is gone are dropped when restored. Without `layout` (versions 1
//! and 2) the window opens the unified Inbox.
//!
//! `address_book` (version 5) is the Address Book space's selected book (`book`, the
//! backend's book id; All Contacts without it) and its order (`sort`: `"name"` or
//! `"last_used"`). Earlier files have none: All Contacts by name.
//!
//! Version 4 made the Unread column a default one. A file from an earlier version that
//! lists its own `columns` gets `unread` added once, when it is read; from then on the
//! column stays off if it's turned off ([`ListState::migrate`]).
//!
//! What isn't in `list` follows the defaults from `settings.json`
//! (`thread_list.view`, `thread_list.threaded`, `thread_list.sort`), [`ListDefaults`]:
//! every field is optional, so `list` only records what was changed. `sort` is
//! `"<column>_<asc|desc>"` ([`ListSort`]); the date-only names of earlier versions
//! (`"newest_first"`, `"oldest_first"`) still read. Version 1 kept a `folders` map with
//! an entry per folder; it is ignored. Unknown fields and an
//! unreadable file are ignored (the file is rewritten on the next change); writes go
//! to a temporary file that is renamed into place.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use mazemail_types::{AccountId, FolderId};

use ui::panes::SerializedLayout;

use crate::source::{ContactSort, ListSort, SortBy};

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
    /// Date.
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

    /// The default set: Star · Unread · Subject · Correspondents · Date (CONCEPT §8.2).
    pub const DEFAULT: [Self; 5] = [
        Self::Star,
        Self::Unread,
        Self::Subject,
        Self::Correspondents,
        Self::Date,
    ];

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
            // Room for the icon and the sort arrow next to it.
            Self::Star | Self::Unread | Self::Attachment => 44.,
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

    /// What clicking the column's header sorts by.
    pub fn sort_by(self) -> SortBy {
        match self {
            Self::Star => SortBy::Star,
            Self::Unread => SortBy::Unread,
            Self::Attachment => SortBy::Attachment,
            Self::Subject => SortBy::Subject,
            Self::Correspondents => SortBy::Correspondents,
            Self::Sender => SortBy::Sender,
            Self::Recipients => SortBy::Recipients,
            Self::Tags => SortBy::Tags,
            Self::Account => SortBy::Account,
            Self::Size => SortBy::Size,
            Self::Date => SortBy::Date,
        }
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
    /// `thread_list.sort`.
    pub sort: ListSort,
}

impl Default for ListDefaults {
    fn default() -> Self {
        Self {
            mode: ListMode::Cards,
            threaded: true,
            sort: ListSort::NEWEST_FIRST,
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
    /// The order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<ListSort>,
    /// Table columns that are on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<ColumnId>>,
    /// Column widths the user set, in logical pixels.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub widths: BTreeMap<ColumnId, f32>,
}

impl ListState {
    /// Brings a list state written by format `version` up to [`UI_STATE_VERSION`]:
    /// before version 4 the Unread column wasn't a default one, so a saved set of
    /// columns gets it added (once; the file is written as version 4 from then on).
    fn migrate(&mut self, version: u32) {
        if version < 4
            && let Some(columns) = &mut self.columns
            && !columns.contains(&ColumnId::Unread)
        {
            columns.push(ColumnId::Unread);
            columns.sort_unstable();
        }
    }
}

/// The effective list settings: the remembered state over the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct ListPrefs {
    /// Cards or table.
    pub mode: ListMode,
    /// Threads on or off.
    pub threaded: bool,
    /// The order: any column, either way.
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

/// What the Address Book space remembers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AddressBookState {
    /// The selected book; All Contacts when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book: Option<String>,
    /// The contact list's order.
    pub sort: ContactSort,
}

impl AddressBookState {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// The format version written now.
pub const UI_STATE_VERSION: u32 = 5;

/// The contents of `ui_state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// Format version, [`UI_STATE_VERSION`].
    pub version: u32,
    /// The list view, the same in every folder.
    pub list: ListState,
    /// Quick Move targets per account (keyed by its public id, `a2`), newest first.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub move_targets: BTreeMap<String, Vec<FolderId>>,
    /// The center's panes and tabs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<SerializedLayout>,
    /// The Address Book's book and order.
    #[serde(skip_serializing_if = "AddressBookState::is_default")]
    pub address_book: AddressBookState,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            version: UI_STATE_VERSION,
            list: ListState::default(),
            move_targets: BTreeMap::new(),
            layout: None,
            address_book: AddressBookState::default(),
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
        let mut state = match std::fs::read(&path) {
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
        state.list.migrate(state.version);
        state.version = UI_STATE_VERSION;
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

    /// The center's panes and tabs, as saved last.
    pub fn layout(&self) -> Option<SerializedLayout> {
        self.lock().state.layout.clone()
    }

    /// Remembers the center's panes and tabs and writes the file (if there is one and
    /// the layout changed).
    pub fn set_layout(&self, layout: SerializedLayout) {
        if self.lock().state.layout.as_ref() == Some(&layout) {
            return;
        }
        self.change(|inner| inner.state.layout = Some(layout));
    }

    /// The Address Book's book and order, as saved last.
    pub fn address_book(&self) -> AddressBookState {
        self.lock().state.address_book.clone()
    }

    /// Remembers the Address Book's book and order and writes the file (if there is
    /// one and they changed).
    pub fn set_address_book(&self, state: AddressBookState) {
        if self.lock().state.address_book == state {
            return;
        }
        self.change(|inner| inner.state.address_book = state);
    }

    /// Changes the state and writes the file (if there is one).
    fn change(&self, edit: impl FnOnce(&mut Inner)) {
        let snapshot = {
            let mut inner = self.lock();
            edit(&mut inner);
            inner.state.version = UI_STATE_VERSION;
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
            sort: ListSort::NEWEST_FIRST,
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
        prefs.sort = ListSort {
            by: SortBy::Subject,
            descending: true,
        };
        prefs.toggle_column(ColumnId::Size);
        prefs.toggle_column(ColumnId::Star);
        prefs.toggle_column(ColumnId::Subject);
        prefs.widths.insert(ColumnId::Correspondents, 220.);
        store.set_prefs(&prefs);
        assert_eq!(
            prefs.columns,
            vec![
                ColumnId::Unread,
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
        assert!(json.contains("\"subject_desc\""), "{json}");

        // Back to the defaults: nothing is recorded.
        store.set_prefs(&ListPrefs::resolve(None, &defaults));
        assert_eq!(store.state().list, ListState::default());
    }

    #[test]
    fn the_date_only_sort_names_still_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        std::fs::write(
            &path,
            r#"{ "version": 2, "list": { "view": "table", "sort": "oldest_first" } }"#,
        )
        .unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(store.prefs().sort, ListSort::OLDEST_FIRST);
        assert_eq!(store.prefs().mode, ListMode::Table);
        // Every column sorts by itself.
        for column in ColumnId::ALL {
            assert_eq!(column.sort_by().key(), column.key());
        }
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
    fn the_layout_survives_a_restart_and_older_files_still_load() {
        use ui::panes::{SerializedAxis, SerializedItem, SerializedMember, SerializedPane};

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        // Version 2: no layout, the rest reads as before.
        std::fs::write(&path, r#"{ "version": 2, "list": { "view": "table" } }"#).unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(store.layout(), None);
        assert_eq!(store.prefs().mode, ListMode::Table);

        let layout = SerializedLayout {
            root: SerializedMember::Split {
                axis: SerializedAxis::Horizontal,
                members: vec![
                    SerializedMember::Pane(SerializedPane {
                        items: vec![SerializedItem::new(
                            "mail",
                            serde_json::json!({ "type": "unified", "role": "inbox" }),
                        )],
                        active: 0,
                    }),
                    SerializedMember::Pane(SerializedPane {
                        items: vec![
                            SerializedItem::new("compose", serde_json::json!({ "draft": 17 }))
                                .with_title("brave-otter"),
                        ],
                        active: 0,
                    }),
                ],
                flexes: vec![1.5, 1.],
            },
            active_pane: 1,
        };
        store.set_layout(layout.clone());
        let reopened = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(reopened.layout(), Some(layout));
        assert_eq!(reopened.state().version, UI_STATE_VERSION);
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"brave-otter\""), "{json}");
        assert!(json.contains("\"version\": 5"), "{json}");
    }

    #[test]
    fn the_address_books_book_and_order_survive_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        // Version 4 had no Address Book: All Contacts by name.
        std::fs::write(&path, r#"{ "version": 4, "list": {} }"#).unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(store.address_book(), AddressBookState::default());
        store.set_address_book(AddressBookState {
            book: Some("collected".into()),
            sort: ContactSort::LastUsed,
        });
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(json.contains("\"last_used\""), "{json}");
        let reopened = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(
            reopened.address_book(),
            AddressBookState {
                book: Some("collected".into()),
                sort: ContactSort::LastUsed,
            }
        );
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
            vec![ColumnId::Unread, ColumnId::Subject, ColumnId::Date]
        );
    }

    #[test]
    fn saved_columns_from_before_version_4_get_the_unread_column_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(UI_STATE_FILE);
        std::fs::write(
            &path,
            r#"{ "version": 3, "list": { "view": "table",
                 "columns": ["star", "subject", "correspondents", "size", "date"],
                 "widths": { "correspondents": 220.0 } } }"#,
        )
        .unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        let mut prefs = store.prefs();
        assert_eq!(
            prefs.columns,
            vec![
                ColumnId::Star,
                ColumnId::Unread,
                ColumnId::Subject,
                ColumnId::Correspondents,
                ColumnId::Size,
                ColumnId::Date
            ],
            "added after the star; the other choices stay"
        );
        assert_eq!(prefs.width(ColumnId::Correspondents), 220.);
        assert_eq!(prefs.mode, ListMode::Table);

        // Turned off again, it stays off.
        prefs.toggle_column(ColumnId::Unread);
        store.set_prefs(&prefs);
        let reopened = UiStateStore::load(&path, ListDefaults::default());
        assert!(!reopened.prefs().columns.contains(&ColumnId::Unread));
        assert_eq!(reopened.state().version, UI_STATE_VERSION);

        // The old default set wasn't saved, so it simply becomes the new default.
        std::fs::write(&path, r#"{ "version": 3, "list": { "view": "table" } }"#).unwrap();
        let store = UiStateStore::load(&path, ListDefaults::default());
        assert_eq!(store.prefs().columns, ColumnId::DEFAULT);
    }
}

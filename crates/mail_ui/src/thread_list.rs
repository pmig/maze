//! The message list (CONCEPT §8.2): two-line cards or a Thunderbird table, threaded
//! (a thread tree) or one row per message, over a keys-only list of any length.
//!
//! # Data
//!
//! The list holds the keys of the open folder ([`ListKeys`]: thread ids, or message
//! ids with threads off) and a small window of formatted rows around the visible
//! range ([`ListModel`]); rows outside it are never fetched. Expanded threads are
//! spliced into the visible sequence virtually (see [`crate::list_model`]): their reply
//! trees come from [`MailSource::thread_tree`] and only the visible children's rows
//! are fetched.
//!
//! # Expansions and live updates
//!
//! Expansions are remembered by thread id, not by index: when the keys change (mail
//! arrived, moved, a filter changed) each expanded thread is looked up in the new
//! keys and its tree is read again, so expansions and the selection stay put. "Expand
//! all" (`*`) is a mode: it sizes the list from every thread's message count (on a
//! background thread above [`SYNC_EXPAND_ALL`] threads) and fetches reply trees only
//! for the threads that scroll into view; threads collapsed by hand are remembered as
//! exceptions. `\` leaves the mode and collapses everything.
//!
//! # Drawing
//!
//! Cards are a GPUI `uniform_list` of 26 px slots: a card takes two (its two lines), a
//! message of an expanded thread one, as a compact indented line with tree guides. The
//! table is gpui-kit's `DataTable` with 24 px rows; the reply tree lives in its Subject
//! column. Both read the same [`ListModel`]. These sizes are at the default font size;
//! rows, indents and the other fixed lengths scale with the `font_size` setting
//! ([`ui::theme::scaled`]).
//!
//! # Selection
//!
//! One row is selected, or several ([`Selection`]: `shift-up`/`down`, shift-click,
//! cmd-click, `cmd-a`). Row commands (`a`, `m`, `s`, `shift-m`) act on every selected
//! row as one batch: a thread row stands for its thread, a message row for itself. Up
//! to [`MAX_KEPT_ROWS`] selected rows are found again by identity when the list
//! changes; "select all" stays select all.
//!
//! # View
//!
//! Cards or table, threads on or off, the order, the table's columns and their widths
//! are the same in every folder and remembered in [`UiStateStore`] (`ui_state.json`).
//!
//! # Sorting
//!
//! Any column sorts the list: a click on its header (again: the other way round, with
//! an arrow on the sorted column), the sort button in the list header (cards and
//! table), or the `view::SortBy…` actions in the command palette. The source sorts
//! ([`MailSource::thread_keys`]): the view only ever holds keys in display order. Date
//! orders load like an unfiltered folder, at once; other orders in the background
//! ("Sorting…"), keeping the selected row and scrolling to it when they arrive.
//!
//! # Context menu
//!
//! A right-click on a row (cards or table) opens the message menu ([`message_menu`]):
//! reply, mark read or unread, star, archive, move, select the thread. It selects the
//! row first unless the row is part of the selection already, in which case the menu
//! acts on every selected row (without marking anything read). Its items dispatch the
//! window's actions, so they show the list's key bindings and do exactly what the keys
//! do; the message view opens the same menu.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Action, AnyElement, App, AppContext as _, ClickEvent, Context, Div, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Pixels, Render, ScrollStrategy, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled, Subscription, Task, UniformListScrollHandle, WeakEntity, Window, canvas, div,
    prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, Size, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem},
    table::{Column, DataTable, TableDelegate, TableEvent, TableState},
    tooltip::Tooltip,
    v_flex,
};
use mazemail_types::{Flags, FolderId, MessageId, ThreadId};
use time::UtcOffset;
use ui::assets::IconName;
use ui::keymap::shortcut_label;
use ui::theme::{scaled, ui_scale};

use crate::actions::{
    Archive, Delete, Forward, MoveToFolder, OpenMessage, Reply, ReplyAll, ReportJunk, SelectThread,
    ToggleRead, ToggleStar,
};
use crate::format::{DateFormatter, format_size, format_thousands};
use crate::list_model::{Expansions, RowPosition, TreeRow, tree_rows};
use crate::selection::Selection;
use crate::source::{
    Direction, ListSort, MailSource, MessageKeys, MessageRow, SortBy, SourceError, ThreadFilter,
    ThreadKeys, ThreadPredicate, ThreadRow, ThreadScope,
};
pub use crate::ui_state::ListMode;
use crate::ui_state::{ColumnId, ListPrefs, UiStateStore};

/// Height of a two-line card at the default font size (CONCEPT §8.2, PLAN §4.9); see
/// [`card_height`].
pub const CARD_HEIGHT: f32 = 52.;
/// Height of one card slot at the default font size: a card line, or a message of an
/// expanded thread in cards.
pub const SLOT_HEIGHT: f32 = CARD_HEIGHT / 2.;
/// Slots per card.
const CARD_SLOTS: usize = 2;
/// Height of a table row at the default font size; see [`table_row_height`].
pub const TABLE_ROW_HEIGHT: f32 = 24.;
/// Rows fetched beyond the visible range on each side.
const OVERSCAN: usize = 48;
/// Quick Filter typing pause before the list refilters.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(120);
/// "Expand all" reads the message counts on the main thread up to this many threads,
/// on a background thread beyond (a million threads take about 0.4 s in the store).
pub const SYNC_EXPAND_ALL: usize = 20_000;
/// Reply trees kept while "expand all" fetches them for the visible threads.
const TREE_CACHE: usize = 4_096;
/// Indentation per reply level at the default font size.
const INDENT: f32 = 12.;
/// Deeper replies don't indent further.
const MAX_INDENT_LEVELS: u16 = 12;
/// The widest the table makes the list column (the layout caps it too).
const MAX_TABLE_WIDTH: f32 = 720.;

/// Height of one card slot at the current font size.
fn slot_height(cx: &App) -> Pixels {
    scaled(SLOT_HEIGHT, cx)
}

/// Height of a two-line card at the current font size: two whole slots.
pub fn card_height(cx: &App) -> Pixels {
    slot_height(cx) * CARD_SLOTS as f32
}

/// Height of a table row at the current font size.
pub fn table_row_height(cx: &App) -> Pixels {
    scaled(TABLE_ROW_HEIGHT, cx)
}
/// Selected rows found again by identity after the list changed; a larger selection
/// (except select all) collapses to its cursor.
pub const MAX_KEPT_ROWS: usize = 1_000;

/// A row's identity, stable across key reloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemId {
    /// A thread row.
    Thread(ThreadId),
    /// A message: of an expanded thread, or of an unthreaded list.
    Message(MessageId),
}

/// The keys of the open list.
#[derive(Debug, Clone)]
pub enum ListKeys {
    /// Threads on.
    Threads(ThreadKeys),
    /// Threads off: one row per message.
    Messages(MessageKeys),
}

impl ListKeys {
    fn empty() -> Self {
        Self::Threads(Arc::from([]))
    }

    /// Number of keys.
    pub fn len(&self) -> usize {
        match self {
            Self::Threads(keys) => keys.len(),
            Self::Messages(keys) => keys.len(),
        }
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Threads(a), Self::Threads(b)) => Arc::ptr_eq(a, b),
            (Self::Messages(a), Self::Messages(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// What kind of row a [`RowView`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// A thread; expandable with more than one message.
    Thread { count: u32, expanded: bool },
    /// A message of an expanded thread.
    Child {
        depth: u16,
        has_next_sibling: bool,
        lines: u64,
        last: bool,
    },
    /// A message of an unthreaded list.
    Message,
}

/// A row, formatted once when fetched so rendering doesn't allocate.
#[derive(Debug, Clone)]
pub struct RowView {
    /// What the row shows.
    pub item: ItemId,
    /// The row's thread.
    pub thread: ThreadId,
    kind: RowKind,
    sender: SharedString,
    recipients: SharedString,
    correspondents: SharedString,
    outgoing: bool,
    subject: SharedString,
    date: SharedString,
    size: SharedString,
    account: SharedString,
    tags: SharedString,
    count: Option<SharedString>,
    flags: Flags,
    attachment: bool,
}

impl RowView {
    fn from_thread(row: &ThreadRow, expanded: bool, dates: &DateFormatter) -> Self {
        Self {
            item: ItemId::Thread(row.id),
            thread: row.id,
            kind: RowKind::Thread {
                count: row.message_count,
                expanded,
            },
            sender: row.sender.expose().clone(),
            recipients: row.recipients.expose().clone(),
            correspondents: row.correspondents().expose().clone(),
            outgoing: row.outgoing,
            subject: row.subject.expose().clone(),
            date: dates.short(row.date),
            size: format_size(row.size),
            account: row.account.clone(),
            tags: join_tags(&row.tags),
            count: (row.message_count > 1).then(|| row.message_count.to_string().into()),
            flags: row.flags,
            attachment: row.has_attachments,
        }
    }

    fn from_message(row: &MessageRow, kind: RowKind, dates: &DateFormatter) -> Self {
        Self {
            item: ItemId::Message(row.id),
            thread: row.thread,
            kind,
            sender: row.sender.expose().clone(),
            recipients: row.recipients.expose().clone(),
            correspondents: row.correspondents().expose().clone(),
            outgoing: row.outgoing,
            subject: row.subject.expose().clone(),
            date: dates.short(row.date),
            size: format_size(row.size),
            account: row.account.clone(),
            tags: join_tags(&row.tags),
            count: None,
            flags: row.flags,
            attachment: row.has_attachments,
        }
    }

    fn unread(&self) -> bool {
        !self.flags.contains(Flags::SEEN)
    }

    fn starred(&self) -> bool {
        self.flags.contains(Flags::FLAGGED)
    }

    fn expandable(&self) -> bool {
        matches!(self.kind, RowKind::Thread { count, .. } if count > 1)
    }

    fn expanded(&self) -> bool {
        matches!(self.kind, RowKind::Thread { expanded: true, .. })
    }
}

fn join_tags(tags: &[SharedString]) -> SharedString {
    if tags.is_empty() {
        SharedString::default()
    } else {
        tags.iter()
            .map(SharedString::as_ref)
            .collect::<Vec<_>>()
            .join(", ")
            .into()
    }
}

/// The list's keys, expansions, reply trees and a window of formatted rows around the
/// visible range.
pub struct ListModel {
    source: Arc<dyn MailSource>,
    dates: DateFormatter,
    keys: ListKeys,
    expansions: Expansions,
    trees: HashMap<ThreadId, Arc<[TreeRow]>>,
    start: usize,
    rows: Vec<Option<RowView>>,
}

impl std::fmt::Debug for ListModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListModel")
            .field("keys", &self.keys.len())
            .field("expanded", &self.expansions.count())
            .field("start", &self.start)
            .field("rows", &self.rows.len())
            .finish()
    }
}

impl ListModel {
    fn new(source: Arc<dyn MailSource>, dates: DateFormatter) -> Self {
        Self {
            source,
            dates,
            keys: ListKeys::empty(),
            expansions: Expansions::new(),
            trees: HashMap::new(),
            start: 0,
            rows: Vec::new(),
        }
    }

    /// Visible rows.
    pub fn len(&self) -> usize {
        self.expansions.len(self.keys.len())
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The keys.
    pub fn keys(&self) -> &ListKeys {
        &self.keys
    }

    /// The expanded threads.
    pub fn expansions(&self) -> &Expansions {
        &self.expansions
    }

    fn invalidate(&mut self) {
        self.start = 0;
        self.rows.clear();
    }

    fn thread_at(&self, base: usize) -> Option<ThreadId> {
        match &self.keys {
            ListKeys::Threads(keys) => keys.get(base).copied(),
            ListKeys::Messages(_) => None,
        }
    }

    /// The reply tree of a thread, fetched (and cached) if needed.
    fn tree(&mut self, thread: ThreadId) -> Option<Arc<[TreeRow]>> {
        if let Some(tree) = self.trees.get(&thread) {
            return Some(tree.clone());
        }
        let nodes = match self.source.thread_tree(thread) {
            Ok(nodes) => nodes,
            Err(error) => {
                tracing::debug!(%error, "no reply tree");
                return None;
            }
        };
        if self.trees.len() >= TREE_CACHE {
            self.trees.clear();
        }
        let tree: Arc<[TreeRow]> = tree_rows(&nodes).into();
        self.trees.insert(thread, tree.clone());
        Some(tree)
    }

    /// The identity of visible row `v` (child rows need their thread's tree).
    fn item(&mut self, v: usize) -> Option<ItemId> {
        if v >= self.len() {
            return None;
        }
        match (self.expansions.resolve(v), &self.keys) {
            (RowPosition::Base(base), ListKeys::Threads(keys)) => {
                keys.get(base).copied().map(ItemId::Thread)
            }
            (RowPosition::Base(base), ListKeys::Messages(keys)) => {
                keys.get(base).copied().map(ItemId::Message)
            }
            (RowPosition::Child { base, offset }, _) => {
                let thread = self.thread_at(base)?;
                let tree = self.tree(thread)?;
                tree.get(offset)
                    .map(|row| ItemId::Message(row.node.message))
            }
        }
    }

    /// Makes sure `range` is cached, fetching it plus overscan if it isn't.
    fn ensure(&mut self, range: Range<usize>) {
        let end = range.end.min(self.len());
        if range.start >= end {
            return;
        }
        if range.start >= self.start && end <= self.start + self.rows.len() {
            return;
        }
        let fetch_start = range.start.saturating_sub(OVERSCAN);
        let fetch_end = (end + OVERSCAN).min(self.len());
        let mut wanted: Vec<(RowPosition, Option<ItemId>, RowKind)> =
            Vec::with_capacity(fetch_end - fetch_start);
        let mut threads = Vec::new();
        let mut messages = Vec::new();
        for v in fetch_start..fetch_end {
            let position = self.expansions.resolve(v);
            let (item, kind) = match (position, &self.keys) {
                (RowPosition::Base(base), ListKeys::Threads(keys)) => (
                    keys.get(base).copied().map(ItemId::Thread),
                    RowKind::Thread {
                        count: 0,
                        expanded: self.expansions.children_of(base) > 0,
                    },
                ),
                (RowPosition::Base(base), ListKeys::Messages(keys)) => (
                    keys.get(base).copied().map(ItemId::Message),
                    RowKind::Message,
                ),
                (RowPosition::Child { base, offset }, _) => {
                    let children = self.expansions.children_of(base);
                    let tree = self.thread_at(base).and_then(|t| self.tree(t));
                    match tree.as_ref().and_then(|t| t.get(offset)) {
                        Some(row) => (
                            Some(ItemId::Message(row.node.message)),
                            RowKind::Child {
                                depth: row.node.depth,
                                has_next_sibling: row.has_next_sibling,
                                lines: row.lines,
                                last: offset + 1 == children,
                            },
                        ),
                        None => (None, RowKind::Message),
                    }
                }
            };
            match item {
                Some(ItemId::Thread(id)) => threads.push(id),
                Some(ItemId::Message(id)) => messages.push(id),
                None => {}
            }
            wanted.push((position, item, kind));
        }
        let mut thread_rows: HashMap<ThreadId, ThreadRow> = self
            .source
            .thread_rows(&threads)
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        let mut message_rows: HashMap<MessageId, MessageRow> = self
            .source
            .message_rows(&messages)
            .into_iter()
            .map(|row| (row.id, row))
            .collect();
        self.rows = wanted
            .into_iter()
            .map(|(_, item, kind)| match (item?, kind) {
                (ItemId::Thread(id), RowKind::Thread { expanded, .. }) => thread_rows
                    .remove(&id)
                    .map(|row| RowView::from_thread(&row, expanded, &self.dates)),
                (ItemId::Message(id), kind) => message_rows
                    .remove(&id)
                    .map(|row| RowView::from_message(&row, kind, &self.dates)),
                (ItemId::Thread(_), _) => None,
            })
            .collect();
        self.start = fetch_start;
    }

    fn get(&self, ix: usize) -> Option<&RowView> {
        ix.checked_sub(self.start)
            .and_then(|offset| self.rows.get(offset))
            .and_then(Option::as_ref)
    }

    fn row(&mut self, ix: usize) -> Option<&RowView> {
        self.ensure(ix..ix + 1);
        self.get(ix)
    }
}

/// How the table looks: the folder's columns and widths, the order, the thread
/// whose rows are all highlighted (`cmd-shift-a`) and the selected rows when there
/// are several.
#[derive(Debug, Clone)]
struct TableLook {
    columns: Vec<ColumnId>,
    widths: Vec<f32>,
    sort: ListSort,
    selected_thread: Option<ThreadId>,
    selected_rows: Vec<Range<usize>>,
    /// The table's width, measured while painting (0 before the first frame).
    available: f32,
}

/// Room for the vertical scrollbar and the table's trailing column.
const TABLE_CHROME: f32 = 16.;
/// The narrowest the flexible Subject column gets.
const MIN_SUBJECT: f32 = 160.;

impl TableLook {
    fn from_prefs(prefs: &ListPrefs) -> Self {
        Self {
            columns: prefs.columns.clone(),
            widths: prefs.columns.iter().map(|c| prefs.width(*c)).collect(),
            sort: prefs.sort,
            selected_thread: None,
            selected_rows: Vec::new(),
            available: 0.,
        }
    }

    /// The width of every column but the Subject.
    fn fixed_width(&self) -> f32 {
        self.columns
            .iter()
            .zip(&self.widths)
            .filter(|(c, _)| **c != ColumnId::Subject)
            .map(|(_, w)| *w)
            .sum()
    }

    /// The Subject takes what the other columns leave, as in Thunderbird.
    fn subject_width(&self) -> f32 {
        if self.available <= 0. {
            return ColumnId::Subject.default_width();
        }
        (self.available - self.fixed_width() - TABLE_CHROME).max(MIN_SUBJECT)
    }
}

/// Events from the list.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadListEvent {
    /// The selected row changed.
    Selected(Option<ItemId>),
    /// Flags changed or threads disappeared; unread counts need a refresh.
    Changed,
    /// The list switched between cards and table.
    ModeChanged(ListMode),
    /// Table columns or widths changed: the list may need more room.
    ColumnsChanged,
    /// A row was double-clicked: open its message in a tab of its own.
    Open(ItemId),
}

/// What "expand all" or individual expansions keep.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpandMode {
    /// These threads are expanded.
    Some(HashSet<ThreadId>),
    /// Every thread is expanded except these.
    All { collapsed: HashSet<ThreadId> },
}

/// The selected row's identity, to find it again after the list changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selected {
    item: ItemId,
    thread: Option<ThreadId>,
    position: RowPosition,
}

/// The rows of a multiple selection, to find them again after the list changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum KeptRows {
    /// One row (or more than [`MAX_KEPT_ROWS`]): only the cursor is kept.
    #[default]
    Cursor,
    /// Every row.
    All,
    /// These rows.
    Rows(Vec<Selected>),
}

/// The items of a row command, as one call to the source.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Batch {
    Threads(Vec<ThreadId>),
    Messages(Vec<MessageId>),
}

/// The message list view.
pub struct ThreadList {
    source: Arc<dyn MailSource>,
    offset: UtcOffset,
    scope: ThreadScope,
    title: SharedString,
    search: Option<String>,
    ui_state: UiStateStore,
    prefs: ListPrefs,
    model: Rc<RefCell<ListModel>>,
    look: Rc<RefCell<TableLook>>,
    selection: Selection,
    selected: Option<Selected>,
    kept: KeptRows,
    expand: ExpandMode,
    expanding: Option<Task<()>>,
    hovered: Option<usize>,
    /// Rows set in the table whose `SelectRow` events are still to come.
    table_echoes: Vec<usize>,
    scroll: UniformListScrollHandle,
    table: Entity<TableState<ThreadTable>>,
    focus: FocusHandle,
    quick_filter: Entity<InputState>,
    flags_filter: ThreadFilter,
    loading: Option<Task<()>>,
    /// Scroll to the selected row when the keys loading now arrive (a new order).
    reveal_on_load: bool,
    /// Tell the window about the selected row when the keys loading now arrive (it
    /// was picked for them: the row after archived mail).
    announce_on_load: bool,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ThreadList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadList").finish_non_exhaustive()
    }
}

impl ThreadList {
    /// A list showing `scope`, remembering its view in `ui_state`.
    pub fn new(
        source: Arc<dyn MailSource>,
        offset: UtcOffset,
        scope: ThreadScope,
        title: SharedString,
        ui_state: UiStateStore,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let dates = DateFormatter::new(offset);
        let model = Rc::new(RefCell::new(ListModel::new(source.clone(), dates)));
        let prefs = ui_state.prefs();
        let look = Rc::new(RefCell::new(TableLook::from_prefs(&prefs)));
        let list = cx.weak_entity();
        let table = cx.new(|cx| {
            TableState::new(
                ThreadTable {
                    model: model.clone(),
                    look: look.clone(),
                    list,
                },
                window,
                cx,
            )
            .col_movable(false)
            .sortable(false)
            .select_on_modifier_click(false)
            // Up at the first row stays there, like the cards (not to the last row).
            .loop_selection(false)
        });
        let quick_filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(format!("Quick filter…  {}", shortcut_label("cmd-f", cx)))
                .clean_on_escape()
        });
        let subscriptions = vec![
            cx.subscribe_in(&table, window, Self::on_table_event),
            cx.subscribe_in(&quick_filter, window, Self::on_quick_filter_event),
        ];
        let mut this = Self {
            source,
            offset,
            scope: scope.clone(),
            title: title.clone(),
            search: None,
            ui_state,
            prefs,
            model,
            look,
            selection: Selection::default(),
            selected: None,
            kept: KeptRows::Cursor,
            expand: ExpandMode::Some(HashSet::new()),
            expanding: None,
            hovered: None,
            table_echoes: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            table,
            focus: cx.focus_handle(),
            quick_filter,
            flags_filter: ThreadFilter::default(),
            loading: None,
            reveal_on_load: false,
            announce_on_load: false,
            _subscriptions: subscriptions,
        };
        this.open(scope, title, None, cx);
        this
    }

    /// Opens a folder (or search results) and selects its first row.
    pub fn open(
        &mut self,
        scope: ThreadScope,
        title: SharedString,
        search: Option<String>,
        cx: &mut Context<'_, Self>,
    ) {
        let mode_before = self.prefs.mode;
        self.prefs = self.ui_state.prefs();
        *self.look.borrow_mut() = TableLook::from_prefs(&self.prefs);
        self.scope = scope;
        self.title = title;
        self.search = search;
        self.expand = ExpandMode::Some(HashSet::new());
        self.expanding = None;
        self.reload(true, cx);
        self.table.update(cx, |table, cx| table.refresh(cx));
        if self.prefs.mode != mode_before {
            cx.emit(ThreadListEvent::ModeChanged(self.prefs.mode));
        }
    }

    /// The open scope.
    pub fn scope(&self) -> &ThreadScope {
        &self.scope
    }

    /// The search query, when the list shows search results.
    pub fn search(&self) -> Option<&str> {
        self.search.as_deref()
    }

    /// Title of the open folder.
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// Number of visible rows (threads, their expanded messages, or messages).
    pub fn len(&self) -> usize {
        self.model.borrow().len()
    }

    /// Number of threads (or, with threads off, messages) in the list.
    pub fn key_count(&self) -> usize {
        self.model.borrow().keys.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.model.borrow().is_empty()
    }

    /// Whether keys are still loading (text filters and orders other than the date run
    /// in the background).
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Whether "expand all" is still reading message counts.
    pub fn is_expanding(&self) -> bool {
        self.expanding.is_some()
    }

    /// The selected index (the cursor, with several rows selected).
    pub fn selected_index(&self) -> Option<usize> {
        self.selection.selected()
    }

    /// Number of selected rows.
    pub fn selected_count(&self) -> usize {
        self.selection.count()
    }

    /// Whether visible row `ix` is selected.
    pub fn is_row_selected(&self, ix: usize) -> bool {
        self.selection.is_selected(ix)
    }

    /// The selected row.
    pub fn selected_item(&self) -> Option<ItemId> {
        self.selected.map(|s| s.item)
    }

    /// The thread of the selected row.
    pub fn selected_thread(&self) -> Option<ThreadId> {
        self.selected.and_then(|s| s.thread)
    }

    /// The thread selected as a whole with `cmd-shift-a`, if any.
    pub fn selected_whole_thread(&self) -> Option<ThreadId> {
        self.look.borrow().selected_thread
    }

    /// The current display mode.
    pub fn mode(&self) -> ListMode {
        self.prefs.mode
    }

    /// Whether threads are on.
    pub fn threaded(&self) -> bool {
        self.prefs.threaded
    }

    /// The order.
    pub fn sort(&self) -> ListSort {
        self.prefs.sort
    }

    /// The folder's list settings.
    pub fn prefs(&self) -> &ListPrefs {
        &self.prefs
    }

    /// The shared list data (tests and benchmarks).
    pub fn model(&self) -> &Rc<RefCell<ListModel>> {
        &self.model
    }

    /// The width the table needs for its columns.
    pub fn table_width(&self) -> f32 {
        let look = self.look.borrow();
        (look.fixed_width() + ColumnId::Subject.default_width() + TABLE_CHROME).min(MAX_TABLE_WIDTH)
    }

    /// The scroll handle of the card list (benchmarks drive it).
    pub fn scroll_handle(&self) -> &UniformListScrollHandle {
        &self.scroll
    }

    /// The table state (benchmarks drive its scroll handle).
    pub fn table(&self) -> &Entity<TableState<ThreadTable>> {
        &self.table
    }

    /// The Quick Filter input.
    pub fn quick_filter(&self) -> &Entity<InputState> {
        &self.quick_filter
    }

    fn filter(&self, cx: &App) -> ThreadFilter {
        let quick = self.quick_filter.read(cx).value();
        let text = [self.search.as_deref().unwrap_or(""), quick.trim()]
            .join(" ")
            .trim()
            .to_owned();
        ThreadFilter {
            text: (!text.is_empty()).then_some(text),
            ..self.flags_filter.clone()
        }
    }

    fn save_prefs(&self) {
        self.ui_state.set_prefs(&self.prefs);
    }

    fn load_keys(
        source: &Arc<dyn MailSource>,
        scope: &ThreadScope,
        filter: &ThreadFilter,
        threaded: bool,
        sort: ListSort,
    ) -> ListKeys {
        let keys = if threaded {
            source
                .thread_keys(scope, filter, sort)
                .map(ListKeys::Threads)
        } else {
            source
                .message_keys(scope, filter, sort)
                .map(ListKeys::Messages)
        };
        keys.unwrap_or_else(|error| {
            tracing::debug!(%error, "no keys");
            if threaded {
                ListKeys::Threads(Arc::from([]))
            } else {
                ListKeys::Messages(Arc::from([]))
            }
        })
    }

    /// Reloads the keys for the current scope and filter. Unfiltered folders in date
    /// order load synchronously (cheap key lists); text filters and other orders load
    /// in the background.
    fn reload(&mut self, select_first: bool, cx: &mut Context<'_, Self>) {
        let filter = self.filter(cx);
        let scope = self.scope.clone();
        let (threaded, sort) = (self.prefs.threaded, self.prefs.sort);
        if filter.text.is_none() && sort.is_by_date() {
            let started = std::time::Instant::now();
            let keys = Self::load_keys(&self.source, &scope, &filter, threaded, sort);
            tracing::debug!(
                elapsed_ms = started.elapsed().as_secs_f64() * 1e3,
                "folder keys loaded"
            );
            self.loading = None;
            self.set_keys(keys, select_first, cx);
            return;
        }
        if select_first {
            // Another folder: its rows, not the last folder's, once they are loaded.
            let empty = if threaded {
                ListKeys::Threads(Arc::from([]))
            } else {
                ListKeys::Messages(Arc::from([]))
            };
            self.set_keys(empty, true, cx);
        }
        let source = self.source.clone();
        let task = cx.background_spawn(async move {
            Self::load_keys(&source, &scope, &filter, threaded, sort)
        });
        self.loading = Some(cx.spawn(async move |this, cx| {
            let keys = task.await;
            this.update(cx, |this, cx| {
                this.loading = None;
                this.set_keys(keys, select_first, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Positions of `threads` in thread keys (a hash map for many, scans for a few).
    fn positions(keys: &[ThreadId], threads: &HashSet<ThreadId>) -> Vec<(usize, ThreadId)> {
        if threads.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<(usize, ThreadId)> = if threads.len() <= 32 {
            threads
                .iter()
                .filter_map(|t| keys.iter().position(|k| k == t).map(|ix| (ix, *t)))
                .collect()
        } else {
            keys.iter()
                .enumerate()
                .filter(|(_, k)| threads.contains(k))
                .map(|(ix, k)| (ix, *k))
                .collect()
        };
        found.sort_unstable_by_key(|(ix, _)| *ix);
        found
    }

    fn set_keys(&mut self, keys: ListKeys, select_first: bool, cx: &mut Context<'_, Self>) {
        let previous = self.selected;
        {
            let mut model = self.model.borrow_mut();
            model.dates = DateFormatter::new(self.offset);
            model.keys = keys;
            model.trees.clear();
            model.expansions = Expansions::new();
            model.invalidate();
            // Expansions follow their threads to their new places, with fresh trees.
            if let (ListKeys::Threads(keys), ExpandMode::Some(expanded)) =
                (model.keys.clone(), &self.expand)
            {
                let mut spans = Vec::new();
                for (base, thread) in Self::positions(&keys, expanded) {
                    if let Some(tree) = model.tree(thread)
                        && tree.len() > 1
                    {
                        spans.push((base, tree.len()));
                    }
                }
                model.expansions = Expansions::from_counts(spans);
            }
        }
        if matches!(self.expand, ExpandMode::All { .. }) {
            self.count_for_expand_all(cx);
        }
        let len = self.len();
        let keep = previous.and_then(|item| self.locate(item, true));
        self.selection.reset(len, keep);
        if select_first || self.selection.selected().is_none() {
            self.selection.first();
            self.kept = KeptRows::Cursor;
        }
        self.restore_rows(true);
        self.table.update(cx, |table, cx| table.refresh(cx));
        let now = self.selection.selected().and_then(|ix| self.item_at(ix));
        if select_first || now.map(|s| s.item) != previous.map(|s| s.item) {
            self.reveal_selection(ScrollStrategy::Top, false, cx);
            self.selection_changed(cx);
        } else if std::mem::take(&mut self.announce_on_load) {
            self.selection_changed(cx);
        } else {
            // Same row selected: the window keeps showing it.
            self.selected = now;
            self.sync_table_row(cx);
            cx.notify();
        }
        self.announce_on_load = false;
        if std::mem::take(&mut self.reveal_on_load) {
            self.reveal_selection(ScrollStrategy::Center, false, cx);
        }
    }

    /// Selects the rows of a multiple selection again after the list changed (the
    /// cursor is placed already). With `search`, moved rows are looked up in the keys.
    fn restore_rows(&mut self, search: bool) {
        match std::mem::take(&mut self.kept) {
            KeptRows::Cursor => {}
            KeptRows::All => self.selection.select_all(),
            KeptRows::Rows(rows) => {
                let found = self.locate_all(&rows, search);
                let cursor = self.selection.selected();
                self.selection.reset_rows(self.len(), cursor, found);
            }
        }
        self.remember_rows();
    }

    /// Remembers the rows of a multiple selection by identity (see [`KeptRows`]).
    fn remember_rows(&mut self) {
        self.kept = if !self.selection.is_multiple() {
            KeptRows::Cursor
        } else if self.selection.is_all() {
            KeptRows::All
        } else if self.selection.count() > MAX_KEPT_ROWS {
            KeptRows::Cursor
        } else {
            let indices: Vec<usize> = self.selection.indices().collect();
            KeptRows::Rows(
                indices
                    .into_iter()
                    .filter_map(|ix| self.item_at(ix))
                    .collect(),
            )
        };
    }

    /// The visible indexes of `rows` (those still in the list). Rows still at their
    /// place are found at once; with `search` the others are looked up in one pass
    /// over the keys.
    fn locate_all(&self, rows: &[Selected], search: bool) -> Vec<usize> {
        let mut found = Vec::with_capacity(rows.len());
        let mut missing = Vec::new();
        for row in rows {
            match self.locate(*row, false) {
                Some(ix) => found.push(ix),
                None => missing.push(*row),
            }
        }
        if !search || missing.is_empty() {
            return found;
        }
        // Where the missing rows' threads (or messages) are now.
        let base_key = |row: &Selected, keys: &ListKeys| match (keys, row.item) {
            (ListKeys::Messages(_), item) => Some(item),
            (ListKeys::Threads(_), ItemId::Thread(id)) => Some(ItemId::Thread(id)),
            (ListKeys::Threads(_), ItemId::Message(_)) => row.thread.map(ItemId::Thread),
        };
        let keys = self.model.borrow().keys.clone();
        let wanted: HashSet<ItemId> = missing.iter().filter_map(|r| base_key(r, &keys)).collect();
        let bases: HashMap<ItemId, usize> = match &keys {
            ListKeys::Threads(keys) => keys
                .iter()
                .enumerate()
                .map(|(ix, id)| (ItemId::Thread(*id), ix))
                .filter(|(item, _)| wanted.contains(item))
                .collect(),
            ListKeys::Messages(keys) => keys
                .iter()
                .enumerate()
                .map(|(ix, id)| (ItemId::Message(*id), ix))
                .filter(|(item, _)| wanted.contains(item))
                .collect(),
        };
        for row in missing {
            let Some(base) = base_key(&row, &keys).and_then(|key| bases.get(&key)) else {
                continue;
            };
            let moved = Selected {
                position: RowPosition::Base(*base),
                ..row
            };
            found.extend(self.locate(moved, false));
        }
        found
    }

    /// The visible index of a row identity. With `search`, a moved thread or message is
    /// looked up in the keys; a message of a collapsed thread resolves to the thread.
    fn locate(&self, selected: Selected, search: bool) -> Option<usize> {
        let mut model = self.model.borrow_mut();
        let base_of = |keys: &ListKeys, hint: usize, item: ItemId| -> Option<usize> {
            match (keys, item) {
                (ListKeys::Threads(keys), ItemId::Thread(id)) => {
                    if keys.get(hint) == Some(&id) {
                        Some(hint)
                    } else if search {
                        keys.iter().position(|k| *k == id)
                    } else {
                        None
                    }
                }
                (ListKeys::Messages(keys), ItemId::Message(id)) => {
                    if keys.get(hint) == Some(&id) {
                        Some(hint)
                    } else if search {
                        keys.iter().position(|k| *k == id)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        let keys = model.keys.clone();
        let hint = selected.position.base();
        match (selected.item, &keys) {
            (ItemId::Thread(_), _) | (ItemId::Message(_), ListKeys::Messages(_)) => {
                let base = base_of(&keys, hint, selected.item)?;
                Some(model.expansions.visible_of(base))
            }
            (ItemId::Message(id), ListKeys::Threads(_)) => {
                let thread = selected.thread?;
                let base = base_of(&keys, hint, ItemId::Thread(thread))?;
                if model.expansions.children_of(base) > 0
                    && let Some(tree) = model.tree(thread)
                    && let Some(offset) = tree.iter().position(|r| r.node.message == id)
                {
                    return Some(model.expansions.visible_of_child(base, offset));
                }
                Some(model.expansions.visible_of(base))
            }
        }
    }

    /// The identity of visible row `ix`.
    /// A double-click on row `ix`: asks for its message in a tab of its own.
    fn open_row(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if let Some(item) = self.item_at(ix).map(|selected| selected.item) {
            cx.emit(ThreadListEvent::Open(item));
        }
    }

    fn item_at(&self, ix: usize) -> Option<Selected> {
        let mut model = self.model.borrow_mut();
        let item = model.item(ix)?;
        let position = model.expansions.resolve(ix);
        let thread = match item {
            ItemId::Thread(id) => Some(id),
            ItemId::Message(_) => model
                .thread_at(position.base())
                .or_else(|| model.row(ix).map(|row| row.thread)),
        };
        Some(Selected {
            item,
            thread,
            position,
        })
    }

    /// Re-reads the keys after mail arrived, moved or changed, keeping the selected
    /// row, the expansions and the scroll position. The window hears
    /// [`ThreadListEvent::Selected`] only if the selected row is gone.
    pub fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        self.reload(false, cx);
    }

    /// Selects only `ix`, marks it read and tells the window.
    pub fn select(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if self.selection.selected() == Some(ix) && !self.selection.is_multiple() {
            return;
        }
        self.selection.select(ix);
        self.reveal_selection(ScrollStrategy::Nearest, false, cx);
        self.selection_changed(cx);
    }

    /// `shift-down` / `shift-up`: extends the selection by `delta` rows.
    pub fn extend(&mut self, delta: isize, cx: &mut Context<'_, Self>) {
        let before = self.selection.selected();
        if self.selection.extend_by(delta) != before {
            self.reveal_selection(ScrollStrategy::Nearest, delta > 0, cx);
            self.selection_changed(cx);
        }
    }

    /// Shift-click: selects the rows from the anchor to `ix`.
    pub fn extend_to(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        self.selection.extend_to(ix);
        self.selection_changed(cx);
    }

    /// Cmd-click: adds `ix` to the selection or takes it out.
    pub fn toggle_row(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        self.selection.toggle(ix);
        self.selection_changed(cx);
    }

    /// `cmd-a`: selects every row.
    pub fn select_all(&mut self, cx: &mut Context<'_, Self>) {
        self.selection.select_all();
        self.selection_changed(cx);
    }

    /// A right-click on row `ix`: selects it, unless it is part of the selection
    /// already (the context menu then acts on the whole selection), as in Finder and
    /// Thunderbird. Unlike a click it doesn't mark the row read, so the menu can.
    pub fn context_click(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if ix >= self.len() || self.selection.is_selected(ix) {
            return;
        }
        self.selection.select(ix);
        self.update_selection(false, cx);
    }

    /// The selection changed: remember it, mark a single selected row read (as
    /// Thunderbird does when it's displayed) and tell the window.
    fn selection_changed(&mut self, cx: &mut Context<'_, Self>) {
        self.update_selection(true, cx);
    }

    fn update_selection(&mut self, mark_read: bool, cx: &mut Context<'_, Self>) {
        self.selected = self.selection.selected().and_then(|ix| self.item_at(ix));
        self.look.borrow_mut().selected_thread = None;
        self.remember_rows();
        if let Some(selected) = self.selected
            && mark_read
            && !self.selection.is_multiple()
        {
            self.mark_read(selected.item, cx);
        }
        self.sync_table_row(cx);
        cx.emit(ThreadListEvent::Selected(self.selected_item()));
        cx.notify();
    }

    /// Shows the selection in the table: the cursor as the table's selected row, the
    /// other selected rows highlighted. The table reports its row back as a
    /// `SelectRow` event later, which must not select again: by then the selection may
    /// have moved on, and two rows would chase each other.
    fn sync_table_row(&mut self, cx: &mut Context<'_, Self>) {
        let rows = if self.selection.is_multiple() {
            self.selection.ranges()
        } else {
            Vec::new()
        };
        if self.look.borrow().selected_rows != rows {
            self.look.borrow_mut().selected_rows = rows;
            self.table.update(cx, |_, cx| cx.notify());
        }
        let (ListMode::Table, Some(ix)) = (self.prefs.mode, self.selection.selected()) else {
            return;
        };
        let echo = self.table.update(cx, |table, cx| {
            if table.selected_row() == Some(ix) {
                return false;
            }
            table.set_selected_row(ix, cx);
            true
        });
        if echo {
            self.table_echoes.push(ix);
        }
    }

    fn reveal_selection(
        &mut self,
        strategy: ScrollStrategy,
        down: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(ix) = self.selection.selected() else {
            return;
        };
        match self.prefs.mode {
            ListMode::Cards => {
                let model = self.model.borrow();
                let slot = model.expansions.slot_of(ix, CARD_SLOTS);
                let card = matches!(model.expansions.resolve(ix), RowPosition::Base(_));
                // Moving down, bring a card's second line into view.
                let slot = if down && card && strategy == ScrollStrategy::Nearest {
                    slot + 1
                } else {
                    slot
                };
                self.scroll.scroll_to_item(slot, strategy);
            }
            ListMode::Table => self
                .table
                .update(cx, |table, cx| table.scroll_to_row(ix, cx)),
        }
    }

    /// Marks a row read when it's displayed, as Thunderbird does.
    fn mark_read(&mut self, item: ItemId, cx: &mut Context<'_, Self>) {
        let unread = {
            let ix = self.selection.selected().unwrap_or(0);
            let mut model = self.model.borrow_mut();
            model
                .row(ix)
                .filter(|row| row.item == item)
                .is_some_and(RowView::unread)
        };
        if unread {
            self.change_flags(&[item], Flags::SEEN, Flags::empty(), cx);
        }
    }

    fn change_flags(
        &mut self,
        items: &[ItemId],
        add: Flags,
        remove: Flags,
        cx: &mut Context<'_, Self>,
    ) {
        let result = match self.batch(items) {
            Batch::Threads(ids) => self.source.set_flags(&ids, add, remove),
            Batch::Messages(ids) => self.source.set_message_flags(&ids, add, remove),
        };
        match result {
            Ok(()) => {
                // Thread rows roll their messages' flags up: refetch the window.
                self.model.borrow_mut().invalidate();
                cx.emit(ThreadListEvent::Changed);
                cx.notify();
            }
            Err(error) => tracing::warn!(%error, "changing flags failed"),
        }
    }

    /// The row a single-row command applies to, with its flags (which decide what
    /// `m` and `s` do): the whole thread after `cmd-shift-a`, else the cursor.
    fn command_target(&mut self) -> Option<(ItemId, Flags)> {
        let ix = self.selection.selected()?;
        if let Some(thread) = self.look.borrow().selected_thread {
            let flags = self
                .source
                .thread_rows(&[thread])
                .first()
                .map_or(Flags::empty(), |row| row.flags);
            return Some((ItemId::Thread(thread), flags));
        }
        let mut model = self.model.borrow_mut();
        model.row(ix).map(|row| (row.item, row.flags))
    }

    /// `f` / down.
    pub fn next(&mut self, cx: &mut Context<'_, Self>) {
        self.step(1, cx);
    }

    /// `b` / up.
    pub fn previous(&mut self, cx: &mut Context<'_, Self>) {
        self.step(-1, cx);
    }

    /// Moves the selection by `delta` rows.
    pub fn step(&mut self, delta: isize, cx: &mut Context<'_, Self>) {
        let before = self.selection.selected();
        if self.selection.step(delta) != before {
            self.reveal_selection(ScrollStrategy::Nearest, delta > 0, cx);
            self.selection_changed(cx);
        }
    }

    /// Moves by a page of rows.
    pub fn page(&mut self, down: bool, cx: &mut Context<'_, Self>) {
        let row_height = match self.prefs.mode {
            ListMode::Cards => card_height(cx),
            ListMode::Table => table_row_height(cx),
        };
        let viewport = self.scroll.0.borrow().base_handle.bounds().size.height;
        let rows = ((viewport / row_height).floor() as isize).max(1);
        self.step(if down { rows } else { -rows }, cx);
    }

    /// Home / end.
    pub fn edge(&mut self, last: bool, cx: &mut Context<'_, Self>) {
        let before = self.selection.selected();
        let after = if last {
            self.selection.last()
        } else {
            self.selection.first()
        };
        if after != before {
            self.reveal_selection(ScrollStrategy::Nearest, last, cx);
            self.selection_changed(cx);
        }
    }

    /// `n` / `p`: next or previous unread thread (with threads off: message); stays
    /// put if there is none.
    pub fn jump_unread(&mut self, direction: Direction, cx: &mut Context<'_, Self>) -> bool {
        let source = self.source.clone();
        let (keys, expansions) = {
            let model = self.model.borrow();
            (model.keys.clone(), model.expansions.clone())
        };
        let from = self
            .selection
            .selected()
            .map(|ix| expansions.resolve(ix).base());
        let found = match &keys {
            ListKeys::Threads(keys) => source.find(keys, from, direction, ThreadPredicate::Unread),
            ListKeys::Messages(keys) => {
                source.find_message(keys, from, direction, ThreadPredicate::Unread)
            }
        };
        let moved = self.selection.jump(direction, |_, _| {
            found.map(|base| expansions.visible_of(base))
        });
        if moved {
            self.reveal_selection(ScrollStrategy::Center, false, cx);
            self.selection_changed(cx);
        }
        moved
    }

    /// `m`: toggles read/unread of the selected rows (or thread, after `cmd-shift-a`);
    /// the cursor row decides which way, as in Thunderbird.
    pub fn toggle_read(&mut self, cx: &mut Context<'_, Self>) {
        if let Some((_, flags)) = self.command_target() {
            let items = self.command_items();
            if flags.contains(Flags::SEEN) {
                self.change_flags(&items, Flags::empty(), Flags::SEEN, cx);
            } else {
                self.change_flags(&items, Flags::SEEN, Flags::empty(), cx);
            }
        }
    }

    /// A click on a row's dot in the table's Unread column: toggles that row's read
    /// state without selecting it, as in Thunderbird (a thread row: all its messages).
    /// Goes the same way as `m`, so `cmd-z` undoes it and it syncs.
    pub fn toggle_read_at(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        let target = self
            .model
            .borrow_mut()
            .row(ix)
            .map(|row| (row.item, row.flags));
        let Some((item, flags)) = target else {
            return;
        };
        if flags.contains(Flags::SEEN) {
            self.change_flags(&[item], Flags::empty(), Flags::SEEN, cx);
        } else {
            self.change_flags(&[item], Flags::SEEN, Flags::empty(), cx);
        }
    }

    /// `s`: toggles the star of the selected rows (or thread, after `cmd-shift-a`).
    pub fn toggle_star(&mut self, cx: &mut Context<'_, Self>) {
        if let Some((_, flags)) = self.command_target() {
            let items = self.command_items();
            if flags.contains(Flags::FLAGGED) {
                self.change_flags(&items, Flags::empty(), Flags::FLAGGED, cx);
            } else {
                self.change_flags(&items, Flags::FLAGGED, Flags::empty(), cx);
            }
        }
    }

    /// `a`: archives the selected rows (or thread, after `cmd-shift-a`) and selects
    /// the next row.
    pub fn archive_selected(&mut self, cx: &mut Context<'_, Self>) {
        let items = self.command_items();
        if items.is_empty() {
            return;
        }
        let batch = self.batch(&items);
        let result = self.remove(
            &items,
            |source| match &batch {
                Batch::Threads(ids) => source.archive(ids),
                Batch::Messages(ids) => source.archive_messages(ids),
            },
            cx,
        );
        if let Err(error) = result {
            tracing::warn!(%error, "archiving failed");
        }
    }

    /// `j`: reports the selected rows (or thread) as junk and selects the next row.
    pub fn junk_selected(&mut self, cx: &mut Context<'_, Self>) {
        let items = self.command_items();
        if items.is_empty() {
            return;
        }
        let batch = self.batch(&items);
        let result = self.remove(
            &items,
            |source| match &batch {
                Batch::Threads(ids) => source.junk(ids),
                Batch::Messages(ids) => source.junk_messages(ids),
            },
            cx,
        );
        if let Err(error) = result {
            tracing::warn!(%error, "reporting junk failed");
        }
    }

    /// `backspace` / `delete`: moves the selected rows to Trash (in Trash: deletes them
    /// for good; the window asks first) and selects the next row.
    pub fn delete_selected(&mut self, cx: &mut Context<'_, Self>) {
        let items = self.command_items();
        if items.is_empty() {
            return;
        }
        let batch = self.batch(&items);
        let result = self.remove(
            &items,
            |source| match &batch {
                Batch::Threads(ids) => source.delete(ids),
                Batch::Messages(ids) => source.delete_messages(ids),
            },
            cx,
        );
        if let Err(error) = result {
            tracing::warn!(%error, "deleting failed");
        }
    }

    /// Undoes the last archive, move, read or star change and shows the list again.
    pub fn undo(&mut self, cx: &mut Context<'_, Self>) {
        match self.source.undo() {
            Ok(true) => {
                self.refresh(cx);
                cx.emit(ThreadListEvent::Changed);
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "undo failed"),
        }
    }

    /// What `a`, `m`, `s` and Quick Move (`shift-m`) act on: every selected row, else
    /// the whole thread after `cmd-shift-a`, else the cursor row. A selected message of
    /// a selected thread is covered by its thread and left out.
    pub fn command_items(&mut self) -> Vec<ItemId> {
        if !self.selection.is_multiple() {
            return self
                .command_target()
                .map(|(item, _)| item)
                .into_iter()
                .collect();
        }
        let indices: Vec<usize> = self.selection.indices().collect();
        let rows: Vec<Selected> = indices
            .into_iter()
            .filter_map(|ix| self.item_at(ix))
            .collect();
        let threads: HashSet<ThreadId> = rows
            .iter()
            .filter_map(|row| match row.item {
                ItemId::Thread(id) => Some(id),
                ItemId::Message(_) => None,
            })
            .collect();
        rows.into_iter()
            .filter(|row| match row.item {
                ItemId::Thread(_) => true,
                ItemId::Message(_) => !row.thread.is_some_and(|t| threads.contains(&t)),
            })
            .map(|row| row.item)
            .collect()
    }

    /// `items` as one source call: threads, or messages. A mix of both becomes the
    /// messages plus the threads' messages in this list, so it stays one command (and
    /// one undo).
    fn batch(&self, items: &[ItemId]) -> Batch {
        let mut threads = Vec::new();
        let mut messages = Vec::new();
        for item in items {
            match item {
                ItemId::Thread(id) => threads.push(*id),
                ItemId::Message(id) => messages.push(*id),
            }
        }
        if messages.is_empty() {
            return Batch::Threads(threads);
        }
        for thread in threads {
            match self.source.thread_tree(thread) {
                Ok(tree) => messages.extend(tree.iter().map(|node| node.message)),
                Err(error) => tracing::debug!(%error, "no reply tree"),
            }
        }
        Batch::Messages(messages)
    }

    /// Quick Move: moves `items` (from [`Self::command_items`]) into `to`, out of
    /// `from` (see [`MailSource::move_threads`]), and selects the next row.
    pub fn move_items(
        &mut self,
        items: &[ItemId],
        to: FolderId,
        from: Option<FolderId>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), SourceError> {
        let batch = self.batch(items);
        self.remove(
            items,
            |source| match &batch {
                Batch::Threads(ids) => source.move_threads(ids, to, from),
                Batch::Messages(ids) => source.move_messages(ids, to, from),
            },
            cx,
        )
    }

    /// Runs `change`, which takes `items` out of the list (archive, move); the row
    /// after the selection takes their place (the one before it at the end), without
    /// passing through the first row.
    fn remove(
        &mut self,
        items: &[ItemId],
        change: impl FnOnce(&dyn MailSource) -> Result<(), SourceError>,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), SourceError> {
        let removed: HashSet<ItemId> = items.iter().copied().collect();
        let threads: HashSet<ThreadId> = items
            .iter()
            .filter_map(|item| match item {
                ItemId::Thread(id) => Some(*id),
                ItemId::Message(_) => None,
            })
            .collect();
        let cursor = self.selection.selected().unwrap_or(0);
        let ranges = self.selection.ranges();
        let first = ranges.first().map_or(cursor, |r| r.start);
        let last = ranges.last().map_or(cursor, |r| r.end - 1);
        // A row that survives: not a removed row, nor a message of a removed thread.
        let survives = |row: &Selected| {
            !removed.contains(&row.item) && !row.thread.is_some_and(|t| threads.contains(&t))
        };
        let next = (last + 1..last + 64)
            .map_while(|ix| self.item_at(ix))
            .find(survives)
            .or_else(|| {
                (first.saturating_sub(64)..first)
                    .rev()
                    .filter_map(|ix| self.item_at(ix))
                    .find(survives)
            });
        change(self.source.as_ref())?;
        self.selection.reset(self.len(), Some(cursor));
        self.kept = KeptRows::Cursor;
        self.selected = next;
        self.reload(false, cx);
        if self.loading.is_some() {
            // Sorted by something else than the date: the row after the removed ones
            // is selected when the new keys are in.
            self.announce_on_load = true;
        } else {
            self.selection_changed(cx);
        }
        cx.emit(ThreadListEvent::Changed);
        Ok(())
    }

    // ---- thread tree ------------------------------------------------------------------

    /// Whether the thread at visible row `ix` is expanded.
    pub fn is_expanded(&self, ix: usize) -> bool {
        let model = self.model.borrow();
        match model.expansions.resolve(ix) {
            RowPosition::Base(base) => model.expansions.children_of(base) > 0,
            RowPosition::Child { .. } => false,
        }
    }

    /// Expands or collapses the thread at visible row `ix` (the twisty).
    pub fn toggle_expanded(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        let position = self.model.borrow().expansions.resolve(ix);
        if let RowPosition::Base(base) = position {
            if self.is_expanded(ix) {
                self.collapse_base(base, cx);
            } else {
                self.expand_base(base, cx);
            }
        }
    }

    /// Expands the thread at `base`; false if it has a single message (or threads are
    /// off).
    fn expand_base(&mut self, base: usize, cx: &mut Context<'_, Self>) -> bool {
        let expanded = {
            let mut model = self.model.borrow_mut();
            let Some(thread) = model.thread_at(base) else {
                return false;
            };
            match model.tree(thread) {
                Some(tree) if tree.len() > 1 => {
                    model.expansions.set(base, tree.len());
                    Some(thread)
                }
                _ => None,
            }
        };
        let Some(thread) = expanded else {
            return false;
        };
        match &mut self.expand {
            ExpandMode::Some(set) => {
                set.insert(thread);
            }
            ExpandMode::All { collapsed } => {
                collapsed.remove(&thread);
            }
        }
        self.relayout(cx);
        true
    }

    fn collapse_base(&mut self, base: usize, cx: &mut Context<'_, Self>) {
        let thread = {
            let mut model = self.model.borrow_mut();
            model.expansions.set(base, 0);
            model.thread_at(base)
        };
        if let Some(thread) = thread {
            match &mut self.expand {
                ExpandMode::Some(set) => {
                    set.remove(&thread);
                }
                ExpandMode::All { collapsed } => {
                    collapsed.insert(thread);
                }
            }
        }
        self.relayout(cx);
    }

    /// The layout changed (keys unchanged): find the selected row again, refresh.
    fn relayout(&mut self, cx: &mut Context<'_, Self>) {
        self.model.borrow_mut().invalidate();
        let previous = self.selected;
        let len = self.len();
        let keep = previous.and_then(|item| self.locate(item, false));
        self.selection.reset(len, keep);
        if self.selection.selected().is_none() && len > 0 {
            self.selection.first();
        }
        self.restore_rows(false);
        self.table.update(cx, |table, cx| table.refresh(cx));
        let now = self.selection.selected().and_then(|ix| self.item_at(ix));
        if now.map(|s| s.item) == previous.map(|s| s.item) {
            self.selected = now;
            self.sync_table_row(cx);
            cx.notify();
        } else {
            // A message of a thread that was collapsed: its thread is selected now.
            self.selection_changed(cx);
        }
    }

    /// `→`: expands the selected thread, or moves into an expanded one.
    pub fn expand_selected(&mut self, cx: &mut Context<'_, Self>) {
        let Some(ix) = self.selection.selected() else {
            return;
        };
        let position = self.model.borrow().expansions.resolve(ix);
        if let RowPosition::Base(base) = position {
            if self.is_expanded(ix) {
                self.step(1, cx);
            } else {
                self.expand_base(base, cx);
            }
        }
    }

    /// `←`: collapses the selected thread, or goes from a message to its thread.
    pub fn collapse_selected(&mut self, cx: &mut Context<'_, Self>) {
        let Some(ix) = self.selection.selected() else {
            return;
        };
        let position = self.model.borrow().expansions.resolve(ix);
        match position {
            RowPosition::Child { base, .. } => {
                let thread_row = self.model.borrow().expansions.visible_of(base);
                self.selection.select(thread_row);
                self.reveal_selection(ScrollStrategy::Nearest, false, cx);
                self.selection_changed(cx);
            }
            RowPosition::Base(base) if self.is_expanded(ix) => self.collapse_base(base, cx),
            RowPosition::Base(_) => {}
        }
    }

    /// `*`: expands every thread (see the [module docs](self)).
    pub fn expand_all(&mut self, cx: &mut Context<'_, Self>) {
        if !self.prefs.threaded {
            return;
        }
        self.expand = ExpandMode::All {
            collapsed: HashSet::new(),
        };
        self.count_for_expand_all(cx);
    }

    /// Reads every thread's message count (in the background for long lists) and
    /// expands the threads with replies.
    fn count_for_expand_all(&mut self, cx: &mut Context<'_, Self>) {
        let ListKeys::Threads(keys) = self.model.borrow().keys.clone() else {
            return;
        };
        if keys.len() <= SYNC_EXPAND_ALL {
            let counts = self.source.message_counts(&keys);
            self.expanding = None;
            self.apply_counts(&keys, &counts, cx);
            return;
        }
        let source = self.source.clone();
        let task = cx.background_spawn({
            let keys = keys.clone();
            async move { source.message_counts(&keys) }
        });
        self.expanding = Some(cx.spawn(async move |this, cx| {
            let counts = task.await;
            this.update(cx, |this, cx| {
                this.expanding = None;
                let current = this.model.borrow().keys.clone();
                if current.same(&ListKeys::Threads(keys.clone()))
                    && matches!(this.expand, ExpandMode::All { .. })
                {
                    this.apply_counts(&keys, &counts, cx);
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn apply_counts(&mut self, keys: &[ThreadId], counts: &[u32], cx: &mut Context<'_, Self>) {
        let ExpandMode::All { collapsed } = &self.expand else {
            return;
        };
        let expansions = Expansions::from_counts(
            keys.iter()
                .zip(counts)
                .enumerate()
                .filter(|(_, (thread, count))| **count > 1 && !collapsed.contains(thread))
                .map(|(base, (_, count))| (base, *count as usize)),
        );
        {
            let mut model = self.model.borrow_mut();
            model.expansions = expansions;
            model.trees.clear();
        }
        self.relayout(cx);
        self.reveal_selection(ScrollStrategy::Center, false, cx);
    }

    /// `\`: collapses every thread.
    pub fn collapse_all(&mut self, cx: &mut Context<'_, Self>) {
        self.expand = ExpandMode::Some(HashSet::new());
        self.expanding = None;
        {
            let mut model = self.model.borrow_mut();
            model.expansions = Expansions::new();
            model.trees.clear();
        }
        self.relayout(cx);
        self.reveal_selection(ScrollStrategy::Center, false, cx);
    }

    /// `cmd-shift-a`: selects the selected row's whole thread; `m`, `s` and `a` then
    /// apply to all its messages, and its rows are highlighted.
    pub fn select_thread(&mut self, cx: &mut Context<'_, Self>) {
        if self.selection.is_multiple()
            && let Some(cursor) = self.selection.selected()
        {
            self.select(cursor, cx);
        }
        let Some(selected) = self.selected else {
            return;
        };
        let Some(thread) = selected.thread else {
            return;
        };
        if self.prefs.threaded {
            // On the thread row: what the message view shows is the thread.
            let base_row = self
                .model
                .borrow()
                .expansions
                .visible_of(selected.position.base());
            if self.selection.selected() != Some(base_row) {
                self.selection.select(base_row);
                self.reveal_selection(ScrollStrategy::Nearest, false, cx);
                self.selection_changed(cx);
            }
        }
        self.look.borrow_mut().selected_thread = Some(thread);
        self.table.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    // ---- view settings ----------------------------------------------------------------

    /// Switches between cards and table (remembered for the folder).
    pub fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.prefs.mode = match self.prefs.mode {
            ListMode::Cards => ListMode::Table,
            ListMode::Table => ListMode::Cards,
        };
        self.save_prefs();
        self.sync_table_row(cx);
        self.reveal_selection(ScrollStrategy::Center, false, cx);
        self.focus(window, cx);
        cx.emit(ThreadListEvent::ModeChanged(self.prefs.mode));
        cx.notify();
    }

    /// Turns threads on or off (remembered for the folder), keeping the selected
    /// conversation in view.
    pub fn toggle_threads(&mut self, cx: &mut Context<'_, Self>) {
        let target = self.selected;
        self.prefs.threaded = !self.prefs.threaded;
        self.save_prefs();
        self.expand = ExpandMode::Some(HashSet::new());
        self.expanding = None;
        // Find the same conversation in the other list: a thread's newest message, or
        // a message's thread.
        self.selected = target.and_then(|t| self.counterpart(t));
        self.reload(false, cx);
        self.reveal_selection(ScrollStrategy::Center, false, cx);
    }

    fn counterpart(&self, selected: Selected) -> Option<Selected> {
        let thread = selected.thread?;
        if self.prefs.threaded {
            return Some(Selected {
                item: ItemId::Thread(thread),
                thread: Some(thread),
                position: RowPosition::Base(0),
            });
        }
        let ids: Vec<MessageId> = match selected.item {
            ItemId::Message(id) => vec![id],
            ItemId::Thread(_) => self
                .source
                .thread_tree(thread)
                .ok()?
                .iter()
                .map(|n| n.message)
                .collect(),
        };
        let newest = self
            .source
            .message_rows(&ids)
            .into_iter()
            .max_by_key(|row| row.date)?;
        Some(Selected {
            item: ItemId::Message(newest.id),
            thread: Some(thread),
            position: RowPosition::Base(0),
        })
    }

    /// Sets the order (the same in every folder), keeping the selected row and
    /// scrolling to it.
    pub fn set_sort(&mut self, sort: ListSort, cx: &mut Context<'_, Self>) {
        if self.prefs.sort == sort {
            return;
        }
        self.prefs.sort = sort;
        self.look.borrow_mut().sort = sort;
        self.save_prefs();
        self.reveal_on_load = true;
        self.reload(false, cx);
        self.table.update(cx, |table, cx| table.refresh(cx));
        cx.notify();
    }

    /// Sorts by `by` (`view::SortBy…`): in its first direction if the list is sorted by
    /// something else, unchanged if it is sorted by `by` already.
    pub fn sort_by(&mut self, by: SortBy, cx: &mut Context<'_, Self>) {
        if self.prefs.sort.by != by {
            self.set_sort(ListSort::by(by), cx);
        }
    }

    /// A click on a column header: sorts by the column, or the other way round if the
    /// list is sorted by it already.
    pub fn sort_by_column(&mut self, column: ColumnId, cx: &mut Context<'_, Self>) {
        self.set_sort(self.prefs.sort.clicked(column.sort_by()), cx);
    }

    /// `view::ReverseSortOrder`.
    pub fn reverse_sort(&mut self, cx: &mut Context<'_, Self>) {
        self.set_sort(self.prefs.sort.toggled(), cx);
    }

    /// Turns a table column on or off (remembered for the folder).
    pub fn toggle_column(&mut self, column: ColumnId, cx: &mut Context<'_, Self>) {
        self.prefs.toggle_column(column);
        self.save_prefs();
        let sort = self.prefs.sort;
        let (selected_thread, selected_rows, available) = {
            let look = self.look.borrow();
            (
                look.selected_thread,
                look.selected_rows.clone(),
                look.available,
            )
        };
        *self.look.borrow_mut() = TableLook {
            sort,
            selected_thread,
            selected_rows,
            available,
            ..TableLook::from_prefs(&self.prefs)
        };
        self.table.update(cx, |table, cx| table.refresh(cx));
        cx.emit(ThreadListEvent::ColumnsChanged);
        cx.notify();
    }

    /// Focuses the list so single-key shortcuts work.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        match self.prefs.mode {
            ListMode::Cards => window.focus(&self.focus, cx),
            ListMode::Table => window.focus(&self.table.focus_handle(cx), cx),
        }
    }

    /// Focuses the Quick Filter (`cmd-f`, `cmd-shift-k`).
    pub fn focus_quick_filter(&self, window: &mut Window, cx: &mut App) {
        let handle = self.quick_filter.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// Whether the Quick Filter has the keyboard focus.
    pub fn quick_filter_focused(&self, window: &Window, cx: &App) -> bool {
        self.quick_filter.focus_handle(cx).is_focused(window)
    }

    fn toggle_flag_filter(
        &mut self,
        which: fn(&mut ThreadFilter) -> &mut bool,
        cx: &mut Context<'_, Self>,
    ) {
        let flag = which(&mut self.flags_filter);
        *flag = !*flag;
        self.reload(false, cx);
    }

    fn on_table_event(
        &mut self,
        _: &Entity<TableState<ThreadTable>>,
        event: &TableEvent,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            TableEvent::SelectRow(ix) => {
                if let Some(pos) = self.table_echoes.iter().position(|e| e == ix) {
                    self.table_echoes.drain(..=pos);
                } else {
                    self.select(*ix, cx);
                }
            }
            TableEvent::DoubleClickedRow(ix) => self.open_row(*ix, cx),
            TableEvent::RightClickedRow(Some(ix)) => {
                let ix = *ix;
                if !self.selection.is_selected(ix) {
                    self.context_click(ix, cx);
                    // Selecting the row cleared the table's right-clicked row, which
                    // its context menu (built next) is for.
                    self.table
                        .update(cx, |table, cx| table.set_right_clicked_row(Some(ix), cx));
                }
            }
            TableEvent::ColumnWidthsChanged(widths) => {
                let columns = self.look.borrow().columns.clone();
                for (column, width) in columns.iter().zip(widths) {
                    if !column.resizable() {
                        continue;
                    }
                    let width = f32::from(*width);
                    if (width - column.default_width()).abs() < 0.5 {
                        self.prefs.widths.remove(column);
                    } else {
                        self.prefs.widths.insert(*column, width);
                    }
                }
                self.look.borrow_mut().widths =
                    columns.iter().map(|c| self.prefs.width(*c)).collect();
                self.save_prefs();
                // The Subject takes up the difference.
                self.table.update(cx, |table, cx| table.refresh(cx));
                cx.emit(ThreadListEvent::ColumnsChanged);
            }
            _ => {}
        }
    }

    fn on_quick_filter_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            InputEvent::Change => {
                // Debounce: replacing the task cancels the previous wait.
                self.loading = Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(FILTER_DEBOUNCE).await;
                    this.update(cx, |this, cx| this.reload(false, cx)).ok();
                }));
            }
            InputEvent::PressEnter { .. } => self.focus(window, cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    // ---- cards ------------------------------------------------------------------------

    fn render_slots(&mut self, range: Range<usize>, cx: &mut Context<'_, Self>) -> Vec<AnyElement> {
        let model = self.model.clone();
        let visible = {
            let model = model.borrow();
            let first = model.expansions.resolve_slot(range.start, CARD_SLOTS).0;
            let last = model
                .expansions
                .resolve_slot(range.end.saturating_sub(1).max(range.start), CARD_SLOTS)
                .0;
            first..last + 1
        };
        model.borrow_mut().ensure(visible);
        let model = model.borrow();
        let selected_thread = self.look.borrow().selected_thread;
        range
            .map(|slot| {
                let (ix, half) = model.expansions.resolve_slot(slot, CARD_SLOTS);
                let row = model.get(ix);
                let highlighted = self.selection.is_selected(ix)
                    || row.is_some_and(|r| Some(r.thread) == selected_thread);
                let slot = match row.map(|r| r.kind) {
                    Some(RowKind::Child { .. }) => {
                        self.render_child_slot(slot, ix, row, highlighted, cx)
                    }
                    _ => self.render_card_slot(slot, ix, half, row, highlighted, cx),
                };
                slot.context_menu(message_menu(cx.weak_entity()))
                    .into_any_element()
            })
            .collect()
    }

    /// The common frame of a slot: height, selection and hover background, accent.
    fn slot_frame(
        &self,
        slot: usize,
        ix: usize,
        highlighted: bool,
        border: bool,
        cx: &mut Context<'_, Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let hovered = self.hovered == Some(ix);
        div()
            .id(slot)
            .h(slot_height(cx))
            .w_full()
            .flex()
            .items_center()
            .relative()
            .overflow_hidden()
            .when(border, |this| {
                this.border_b_1().border_color(theme.border.opacity(0.5))
            })
            .map(|this| {
                if highlighted {
                    this.bg(theme.list_active).child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(theme.primary),
                    )
                } else if hovered {
                    this.bg(theme.list_hover)
                } else {
                    this
                }
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let next = if *hovered {
                    Some(ix)
                } else if this.hovered == Some(ix) {
                    None
                } else {
                    this.hovered
                };
                if next != this.hovered {
                    this.hovered = next;
                    cx.notify();
                }
            }))
            .debug_selector(|| format!("card-row-{ix}"))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    this.focus(window, cx);
                    this.context_click(ix, cx);
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus(window, cx);
                let modifiers = event.modifiers();
                if modifiers.shift {
                    this.extend_to(ix, cx);
                } else if modifiers.secondary() {
                    this.toggle_row(ix, cx);
                } else {
                    this.select(ix, cx);
                    if event.click_count() >= 2 {
                        this.open_row(ix, cx);
                    }
                }
            }))
    }

    fn render_card_slot(
        &self,
        slot: usize,
        ix: usize,
        half: usize,
        row: Option<&RowView>,
        highlighted: bool,
        cx: &mut Context<'_, Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let frame = self
            .slot_frame(slot, ix, highlighted, half == 1, cx)
            .pl_2()
            .pr_3();
        let Some(row) = row else {
            return frame;
        };
        let unread = row.unread();
        if half == 0 {
            return frame.child(
                h_flex()
                    .w_full()
                    .gap_1p5()
                    .pt_1()
                    .child(
                        div()
                            .size(scaled(7., cx))
                            .flex_none()
                            .rounded_full()
                            .when(unread, |this| this.bg(theme.primary)),
                    )
                    .when(row.outgoing, |this| {
                        this.child(
                            Icon::new(IconName::ArrowUpRight)
                                .xsmall()
                                .flex_none()
                                .text_color(theme.muted_foreground),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .when(unread, |this| this.font_semibold())
                            .child(row.correspondents.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if unread {
                                theme.primary
                            } else {
                                theme.muted_foreground
                            })
                            .child(row.date.clone()),
                    ),
            );
        }
        let expandable = row.expandable();
        let expanded = row.expanded();
        let list = cx.weak_entity();
        frame.child(
            h_flex()
                .w_full()
                .gap_1p5()
                .pl(scaled(13., cx))
                .pb_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(if unread {
                            theme.foreground
                        } else {
                            theme.muted_foreground
                        })
                        .child(row.subject.clone()),
                )
                .when_some(row.count.clone(), |this, count| {
                    this.child(
                        h_flex()
                            .id(("twisty", ix))
                            .flex_none()
                            .gap_0p5()
                            .pl_0p5()
                            .pr_1()
                            .rounded_sm()
                            .bg(if expanded {
                                theme.primary.opacity(0.15)
                            } else {
                                theme.muted
                            })
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .hover(|this| this.bg(theme.primary.opacity(0.25)))
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .size_3(),
                            )
                            .child(count)
                            .when(expandable, |this| {
                                this.on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    list.update(cx, |list, cx| list.toggle_expanded(ix, cx))
                                        .ok();
                                })
                            }),
                    )
                })
                .when(row.attachment, |this| {
                    this.child(
                        Icon::new(IconName::Paperclip)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    )
                })
                .when(row.starred(), |this| {
                    this.child(
                        Icon::new(IconName::StarFill)
                            .xsmall()
                            .text_color(theme.warning),
                    )
                }),
        )
    }

    fn render_child_slot(
        &self,
        slot: usize,
        ix: usize,
        row: Option<&RowView>,
        highlighted: bool,
        cx: &mut Context<'_, Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let last = matches!(row.map(|r| r.kind), Some(RowKind::Child { last: true, .. }));
        let frame = self.slot_frame(slot, ix, highlighted, last, cx).pr_3();
        let Some(row) = row else {
            return frame;
        };
        let unread = row.unread();
        frame
            .child(tree_guides(row.kind, 10., theme.border, ui_scale(cx)))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        div()
                            .size(scaled(6., cx))
                            .flex_none()
                            .rounded_full()
                            .when(unread, |this| this.bg(theme.primary)),
                    )
                    .when(row.outgoing, |this| {
                        this.child(
                            Icon::new(IconName::ArrowUpRight)
                                .xsmall()
                                .flex_none()
                                .text_color(theme.muted_foreground),
                        )
                    })
                    .child(
                        div()
                            .min_w_0()
                            .max_w(scaled(160., cx))
                            .truncate()
                            .text_xs()
                            .when(unread, |this| this.font_semibold())
                            .child(row.correspondents.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(row.subject.clone()),
                    )
                    .when(row.attachment, |this| {
                        this.child(
                            Icon::new(IconName::Paperclip)
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        )
                    })
                    .when(row.starred(), |this| {
                        this.child(
                            Icon::new(IconName::StarFill)
                                .xsmall()
                                .text_color(theme.warning),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(row.date.clone()),
                    ),
            )
    }

    // ---- context menu -----------------------------------------------------------------

    /// What the message context menu offers for the selection, in order (`None` is a
    /// separator). The labels follow the row the commands decide on (the cursor, as for
    /// `m` and `s`): "Mark as Unread" for a read row, "Unstar" for a starred one. Empty
    /// without a selection.
    pub fn message_menu_items(&mut self) -> Vec<Option<(SharedString, Box<dyn Action>)>> {
        let Some((_, flags)) = self.command_target() else {
            return Vec::new();
        };
        let entry = |label: &'static str, action: Box<dyn Action>| Some((label.into(), action));
        vec![
            entry("Reply", Box::new(Reply)),
            entry("Reply All", Box::new(ReplyAll)),
            entry("Forward", Box::new(Forward)),
            None,
            if flags.contains(Flags::SEEN) {
                entry("Mark as Unread", Box::new(ToggleRead))
            } else {
                entry("Mark as Read", Box::new(ToggleRead))
            },
            if flags.contains(Flags::FLAGGED) {
                entry("Unstar", Box::new(ToggleStar))
            } else {
                entry("Star", Box::new(ToggleStar))
            },
            entry("Archive", Box::new(Archive)),
            entry("Report Junk", Box::new(ReportJunk)),
            entry("Delete", Box::new(Delete)),
            entry("Move to Folder…", Box::new(MoveToFolder)),
            None,
            entry("Select Thread", Box::new(SelectThread)),
            entry("Open in New Tab", Box::new(OpenMessage)),
        ]
    }

    /// Fills the message context menu (see [`Self::message_menu_items`]).
    pub fn build_message_menu(&mut self, menu: PopupMenu) -> PopupMenu {
        self.message_menu_items()
            .into_iter()
            .fold(menu, |menu, item| match item {
                Some((label, action)) => menu.menu(label, action),
                None => menu.separator(),
            })
    }

    // ---- header -----------------------------------------------------------------------

    fn sort_menu(menu: PopupMenu, sort: ListSort, list: &WeakEntity<Self>) -> PopupMenu {
        let pick = |list: &WeakEntity<Self>, sort: ListSort| {
            let list = list.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                list.update(cx, |list, cx| list.set_sort(sort, cx)).ok();
            }
        };
        let mut menu = menu.label("Sort By");
        for by in SortBy::ALL {
            menu = menu.item(
                PopupMenuItem::new(by.label())
                    .checked(sort.by == by)
                    .on_click(pick(
                        list,
                        if sort.by == by {
                            sort
                        } else {
                            ListSort::by(by)
                        },
                    )),
            );
        }
        let (descending, ascending) = sort.by.direction_labels();
        menu.separator()
            .item(
                PopupMenuItem::new(descending)
                    .checked(sort.descending)
                    .on_click(pick(
                        list,
                        ListSort {
                            descending: true,
                            ..sort
                        },
                    )),
            )
            .item(
                PopupMenuItem::new(ascending)
                    .checked(!sort.descending)
                    .on_click(pick(
                        list,
                        ListSort {
                            descending: false,
                            ..sort
                        },
                    )),
            )
    }

    fn column_menu(
        menu: PopupMenu,
        look: &Rc<RefCell<TableLook>>,
        list: &WeakEntity<Self>,
    ) -> PopupMenu {
        let on: Vec<ColumnId> = look.borrow().columns.clone();
        let mut menu = menu.label("Columns");
        for column in ColumnId::ALL.into_iter().filter(|c| c.hideable()) {
            let list = list.clone();
            menu = menu.item(
                PopupMenuItem::new(column.label())
                    .checked(on.contains(&column))
                    .on_click(move |_, _, cx| {
                        list.update(cx, |list, cx| list.toggle_column(column, cx))
                            .ok();
                    }),
            );
        }
        menu
    }

    fn render_header(&self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let keys = self.key_count() as u64;
        let noun = match (self.prefs.threaded, keys) {
            (true, 1) => "thread",
            (true, _) => "threads",
            (false, 1) => "message",
            (false, _) => "messages",
        };
        let count = if self.loading.is_some() && self.filter(cx).text.is_some() {
            SharedString::from("Searching…")
        } else if self.loading.is_some() && !self.prefs.sort.is_by_date() {
            SharedString::from("Sorting…")
        } else if self.expanding.is_some() {
            SharedString::from("Expanding…")
        } else {
            format!("{} {noun}", format_thousands(keys)).into()
        };
        let filter = self.flags_filter.clone();
        let look = self.look.clone();
        let list = cx.weak_entity();
        let sort = self.prefs.sort;
        let sort_list = list.clone();
        v_flex()
            .flex_none()
            .gap_1p5()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_1()
                    .px_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_semibold()
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .mr_1()
                            .text_color(theme.muted_foreground)
                            .child(count),
                    )
                    .child(
                        Button::new("sort-menu")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ArrowDownUp)
                            .tooltip(format!("Sorted by {}", sort.describe()))
                            .dropdown_menu(move |menu, _, _| {
                                Self::sort_menu(menu, sort, &sort_list)
                            }),
                    )
                    .child(
                        Button::new("toggle-threads")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ListTree)
                            .selected(self.prefs.threaded)
                            .tooltip(if self.prefs.threaded {
                                "Threads on: one row per message instead"
                            } else {
                                "Threads off: group into threads"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_threads(cx))),
                    )
                    .when(self.prefs.mode == ListMode::Table, |this| {
                        this.child(
                            Button::new("table-columns")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Columns3)
                                .tooltip("Columns")
                                .dropdown_menu(move |menu, _, _| {
                                    Self::column_menu(menu, &look, &list)
                                }),
                        )
                    })
                    .child(
                        Button::new("toggle-table-view")
                            .ghost()
                            .xsmall()
                            .icon(match self.prefs.mode {
                                ListMode::Cards => IconName::Table,
                                ListMode::Table => IconName::Rows3,
                            })
                            .tooltip(match self.prefs.mode {
                                ListMode::Cards => "Table view",
                                ListMode::Table => "Card view",
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_mode(window, cx)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .key_context("QuickFilter")
                    .child(
                        div().flex_1().child(
                            Input::new(&self.quick_filter)
                                .small()
                                .cleanable(true)
                                .prefix(
                                    Icon::new(IconName::ListFilter)
                                        .xsmall()
                                        .text_color(theme.muted_foreground),
                                ),
                        ),
                    )
                    .child(
                        Button::new("filter-unread")
                            .ghost()
                            .small()
                            .icon(IconName::MailOpen)
                            .selected(filter.unread)
                            .tooltip("Unread")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_flag_filter(|f| &mut f.unread, cx)
                            })),
                    )
                    .child(
                        Button::new("filter-starred")
                            .ghost()
                            .small()
                            .icon(IconName::Star)
                            .selected(filter.starred)
                            .tooltip("Starred")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_flag_filter(|f| &mut f.starred, cx)
                            })),
                    )
                    .child(
                        Button::new("filter-attachments")
                            .ghost()
                            .small()
                            .icon(IconName::Paperclip)
                            .selected(filter.attachments)
                            .tooltip("Has attachment")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.toggle_flag_filter(|f| &mut f.attachments, cx)
                            })),
                    ),
            )
    }
}

/// The builder of the message context menu of `list`'s selection, for
/// `context_menu` on a row or on the message view.
pub fn message_menu(
    list: WeakEntity<ThreadList>,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<'_, PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, cx| match list.upgrade() {
        Some(list) => list.update(cx, |list, _| list.build_message_menu(menu)),
        None => menu,
    }
}

/// The tree guides of a child row (Thunderbird's tree lines) and its left padding;
/// `left` is where level 0 starts. Thread and message rows get the padding only.
/// `left` and the indent are at the default font size and grow by `scale`
/// ([`ui::theme::ui_scale`]); the lines stay 1 px.
fn tree_guides(kind: RowKind, left: f32, color: Hsla, scale: f32) -> Div {
    let left = (left * scale).round();
    let RowKind::Child {
        depth,
        has_next_sibling,
        lines,
        ..
    } = kind
    else {
        return div().flex_none().w(px(left));
    };
    let indent = (INDENT * scale).round();
    let levels = depth.min(MAX_INDENT_LEVELS);
    let width = left + f32::from(levels + 1) * indent + 2.;
    let line = |x: f32| div().absolute().left(px(x)).w(px(1.)).bg(color);
    let mut guides = div().flex_none().relative().h_full().w(px(width));
    for level in 0..levels {
        if lines & (1 << u64::from(level).min(63)) != 0 {
            guides = guides.child(
                line(left + f32::from(level) * indent + indent / 2.)
                    .top_0()
                    .bottom_0(),
            );
        }
    }
    let x = left + f32::from(levels) * indent + indent / 2.;
    guides
        .child(
            line(x)
                .top_0()
                .when(has_next_sibling, |this| this.bottom_0())
                .when(!has_next_sibling, |this| this.h(gpui::relative(0.5))),
        )
        .child(
            div()
                .absolute()
                .left(px(x))
                .top(gpui::relative(0.5))
                .w(px(indent / 2. + 1.))
                .h(px(1.))
                .bg(color),
        )
}

impl EventEmitter<ThreadListEvent> for ThreadList {}

impl Focusable for ThreadList {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.prefs.mode {
            ListMode::Cards => self.focus.clone(),
            ListMode::Table => self.table.focus_handle(cx),
        }
    }
}

impl Render for ThreadList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let empty = self.is_empty() && self.loading.is_none();
        let slots = {
            let model = self.model.borrow();
            model.expansions.slot_len(model.keys.len(), CARD_SLOTS)
        };
        let body = match self.prefs.mode {
            ListMode::Cards => div()
                .id("thread-cards")
                .size_full()
                .track_focus(&self.focus)
                .child(
                    uniform_list(
                        "threads",
                        slots,
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            this.render_slots(range, cx)
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.scroll),
                )
                .into_any_element(),
            ListMode::Table => {
                let look = self.look.clone();
                let table = self.table.clone();
                div()
                    .size_full()
                    .relative()
                    .child(
                        DataTable::new(&self.table)
                            .with_size(Size::Size(table_row_height(cx)))
                            .stripe(false)
                            .bordered(false),
                    )
                    .child(
                        // Measures the table so the Subject column can flex.
                        canvas(
                            move |bounds, _, cx| {
                                let width = f32::from(bounds.size.width);
                                let changed = {
                                    let mut look = look.borrow_mut();
                                    let changed = (look.available - width).abs() > 0.5;
                                    look.available = width;
                                    changed
                                };
                                if changed {
                                    cx.defer(move |cx| {
                                        table.update(cx, |table, cx| table.refresh(cx));
                                    });
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(self.render_header(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .key_context("ThreadList")
                    .when(empty, |this| {
                        this.child(
                            v_flex()
                                .size_full()
                                .items_center()
                                .justify_center()
                                .gap_3()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .px_6()
                                .text_center()
                                .child(
                                    self.source
                                        .placeholder()
                                        .unwrap_or_else(|| SharedString::from("No messages")),
                                )
                                .when(self.source.offers_add_account(), |this| {
                                    this.child(
                                        Button::new("empty-add-account")
                                            .primary()
                                            .small()
                                            .icon(IconName::Plus)
                                            .label("Add Account…")
                                            .on_click(|_, window, cx| {
                                                window.dispatch_action(
                                                    Box::new(crate::actions::AddAccount),
                                                    cx,
                                                );
                                            }),
                                    )
                                }),
                        )
                    })
                    .when(!empty, |this| this.child(body)),
            )
    }
}

/// The table view's delegate: the folder's columns over the shared [`ListModel`].
pub struct ThreadTable {
    model: Rc<RefCell<ListModel>>,
    look: Rc<RefCell<TableLook>>,
    list: WeakEntity<ThreadList>,
}

impl std::fmt::Debug for ThreadTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadTable").finish_non_exhaustive()
    }
}

/// The list width the default table columns need.
pub const TABLE_LIST_WIDTH: f32 = 640.;

impl ThreadTable {
    fn column_id(&self, col_ix: usize) -> ColumnId {
        self.look
            .borrow()
            .columns
            .get(col_ix)
            .copied()
            .unwrap_or(ColumnId::Subject)
    }
}

impl TableDelegate for ThreadTable {
    fn columns_count(&self, _: &App) -> usize {
        self.look.borrow().columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.model.borrow().len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let look = self.look.borrow();
        let id = look
            .columns
            .get(col_ix)
            .copied()
            .unwrap_or(ColumnId::Subject);
        let width = if id == ColumnId::Subject {
            look.subject_width()
        } else {
            look.widths
                .get(col_ix)
                .copied()
                .unwrap_or_else(|| id.default_width())
        };
        let column = Column::new(id.key(), id.label())
            .width(px(width))
            .resizable(id.resizable());
        match id {
            ColumnId::Star | ColumnId::Unread | ColumnId::Attachment => column.text_center(),
            ColumnId::Size => column.text_right(),
            ColumnId::Subject => column.min_width(px(MIN_SUBJECT)),
            _ => column.min_width(px(48.)),
        }
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _: &mut Window,
        _: &mut Context<'_, TableState<Self>>,
    ) {
        self.model.borrow_mut().ensure(visible_range);
    }

    fn context_menu(
        &mut self,
        _: usize,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> PopupMenu {
        // The right-clicked row is selected (or part of the selection) by now.
        match self.list.upgrade() {
            Some(list) => list.update(cx, |list, _| list.build_message_menu(menu)),
            None => menu,
        }
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let id = self.column_id(col_ix);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let look = self.look.clone();
        let list = self.list.clone();
        let sort = self.look.borrow().sort;
        let sorted = sort.by == id.sort_by();
        let arrow = |color| {
            Icon::new(if sort.descending {
                IconName::ChevronDown
            } else {
                IconName::ChevronUp
            })
            .size_3()
            .flex_none()
            .text_color(color)
        };
        let icon = match id {
            ColumnId::Star => Some(IconName::Star),
            ColumnId::Unread => Some(IconName::CircleDot),
            ColumnId::Attachment => Some(IconName::Paperclip),
            _ => None,
        };
        let content = h_flex()
            .id(("th", col_ix))
            .debug_selector(move || format!("th-{}", id.key()))
            .size_full()
            .min_w_0()
            .gap_1()
            .cursor_pointer()
            .map(|this| match icon {
                // Icon columns keep their icon and add the arrow while sorted.
                Some(icon) => this
                    .justify_center()
                    .child(Icon::new(icon).xsmall().text_color(muted))
                    .when(sorted, |this| this.child(arrow(theme.foreground))),
                None => this
                    .child(div().min_w_0().truncate().text_xs().child(id.label()))
                    .when(sorted, |this| this.child(arrow(muted))),
            })
            .tooltip(move |window, cx| {
                let text = if sorted {
                    format!("Sorted by {}", sort.describe())
                } else {
                    format!("Sort by {}", id.label())
                };
                Tooltip::new(text).build(window, cx)
            })
            .on_click({
                let list = list.clone();
                move |_, _, cx| {
                    cx.stop_propagation();
                    list.update(cx, |list, cx| list.sort_by_column(id, cx)).ok();
                }
            });
        content.context_menu(move |menu, _, _| ThreadList::column_menu(menu, &look, &list))
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> Stateful<Div> {
        let (selected_thread, selected) = {
            let look = self.look.borrow();
            let ix = look.selected_rows.partition_point(|r| r.end <= row_ix);
            let selected = look
                .selected_rows
                .get(ix)
                .is_some_and(|r| r.contains(&row_ix));
            (look.selected_thread, selected)
        };
        let highlighted = selected
            || selected_thread.is_some_and(|thread| {
                self.model
                    .borrow()
                    .get(row_ix)
                    .is_some_and(|row| row.thread == thread)
            });
        let list = self.list.clone();
        div()
            .id(("row", row_ix))
            .debug_selector(|| format!("table-row-{row_ix}"))
            .when(highlighted, |this| this.bg(cx.theme().list_active))
            // The table selects on plain clicks; shift and cmd clicks are ours.
            .on_click(move |event, _, cx| {
                let modifiers = event.modifiers();
                if modifiers.shift {
                    list.update(cx, |list, cx| list.extend_to(row_ix, cx)).ok();
                } else if modifiers.secondary() {
                    list.update(cx, |list, cx| list.toggle_row(row_ix, cx)).ok();
                }
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let id = self.column_id(col_ix);
        let mut model = self.model.borrow_mut();
        let Some(row) = model.row(row_ix) else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let unread = row.unread();
        let cell = div()
            .text_xs()
            .truncate()
            .when(unread, |this| this.font_semibold());
        match id {
            ColumnId::Star => cell
                .when(row.starred(), |this| {
                    this.child(
                        Icon::new(IconName::StarFill)
                            .xsmall()
                            .text_color(theme.warning),
                    )
                })
                .into_any_element(),
            // The cards' dot, as a button: a filled dot for unread rows, a faint ring
            // on hover for read ones; a click toggles the row (not the selection).
            ColumnId::Unread => {
                const GROUP: &str = "unread-cell";
                let list = self.list.clone();
                let ring = theme.muted_foreground.opacity(0.6);
                h_flex()
                    .id(("unread", row_ix))
                    .debug_selector(move || format!("unread-{row_ix}"))
                    .group(GROUP)
                    .size_full()
                    .justify_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .size(scaled(7., cx))
                            .flex_none()
                            .rounded_full()
                            .map(|this| {
                                if unread {
                                    this.debug_selector(move || format!("unread-dot-{row_ix}"))
                                        .bg(theme.primary)
                                } else {
                                    this.border_1()
                                        .border_color(gpui::transparent_black())
                                        .group_hover(GROUP, |this| this.border_color(ring))
                                }
                            }),
                    )
                    .tooltip(move |window, cx| {
                        Tooltip::new(if unread {
                            "Mark as Read"
                        } else {
                            "Mark as Unread"
                        })
                        .build(window, cx)
                    })
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        list.update(cx, |list, cx| list.toggle_read_at(row_ix, cx))
                            .ok();
                    })
                    .into_any_element()
            }
            ColumnId::Attachment => cell
                .when(row.attachment, |this| {
                    this.child(
                        Icon::new(IconName::Paperclip)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    )
                })
                .into_any_element(),
            ColumnId::Subject => {
                let expandable = row.expandable();
                let expanded = row.expanded();
                let list = self.list.clone();
                let lead = match row.kind {
                    RowKind::Child { .. } => {
                        tree_guides(row.kind, 0., theme.border.opacity(0.9), ui_scale(cx))
                            .into_any_element()
                    }
                    RowKind::Thread { .. } if expandable => h_flex()
                        .id(("twisty", row_ix))
                        .flex_none()
                        .w(scaled(16., cx))
                        .h_full()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .size_3(),
                        )
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            list.update(cx, |list, cx| list.toggle_expanded(row_ix, cx))
                                .ok();
                        })
                        .into_any_element(),
                    RowKind::Thread { .. } => {
                        div().flex_none().w(scaled(16., cx)).into_any_element()
                    }
                    RowKind::Message => div().into_any_element(),
                };
                h_flex()
                    .h_full()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .child(lead)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .when(unread, |this| this.font_semibold())
                            .when(matches!(row.kind, RowKind::Child { .. }), |this| {
                                this.text_color(theme.muted_foreground)
                            })
                            .child(row.subject.clone()),
                    )
                    .when_some(row.count.clone(), |this, count| {
                        this.child(
                            div()
                                .flex_none()
                                .px_1()
                                .rounded_sm()
                                .bg(theme.muted)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(count),
                        )
                    })
                    .into_any_element()
            }
            ColumnId::Correspondents => h_flex()
                .min_w_0()
                .gap_1()
                .when(row.outgoing, |this| {
                    this.child(
                        Icon::new(IconName::ArrowUpRight)
                            .xsmall()
                            .flex_none()
                            .text_color(theme.muted_foreground),
                    )
                })
                .child(cell.min_w_0().child(row.correspondents.clone()))
                .into_any_element(),
            ColumnId::Sender => cell.child(row.sender.clone()).into_any_element(),
            ColumnId::Recipients => cell.child(row.recipients.clone()).into_any_element(),
            ColumnId::Tags => cell
                .text_color(theme.muted_foreground)
                .child(row.tags.clone())
                .into_any_element(),
            ColumnId::Account => cell
                .text_color(theme.muted_foreground)
                .child(row.account.clone())
                .into_any_element(),
            ColumnId::Size => cell
                .w_full()
                .text_right()
                .text_color(theme.muted_foreground)
                .child(row.size.clone())
                .into_any_element(),
            ColumnId::Date => cell
                .text_color(theme.muted_foreground)
                .child(row.date.clone())
                .into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use mazemail_types::{FolderId, FolderRole};
    use time::macros::datetime;

    use super::*;
    use crate::fake::FakeMailSource;
    use crate::source::FolderKey;
    use crate::ui_state::ListDefaults;

    fn source() -> Arc<dyn MailSource> {
        Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)))
    }

    /// Gmail (personal) / Inbox: 4,200 threads.
    fn inbox() -> ThreadScope {
        ThreadScope::Folder(FolderKey::Folder(FolderId(1)))
    }

    fn open_list<'a>(
        cx: &'a mut gpui::TestAppContext,
        source: &Arc<dyn MailSource>,
        scope: ThreadScope,
        ui_state: UiStateStore,
    ) -> (Entity<ThreadList>, &'a mut gpui::VisualTestContext) {
        cx.update(ui::init);
        let source = source.clone();
        cx.add_window_view(move |window, cx| {
            ThreadList::new(
                source,
                UtcOffset::UTC,
                scope,
                "Inbox".into(),
                ui_state,
                window,
                cx,
            )
        })
    }

    fn memory() -> UiStateStore {
        UiStateStore::in_memory(ListDefaults::default())
    }

    /// The visible index of the first thread with at least `n` messages.
    fn thread_with(list: &ThreadList, source: &Arc<dyn MailSource>, n: u32) -> usize {
        let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
            panic!("threaded");
        };
        let rows = source.thread_rows(&keys[..200]);
        let base = rows.iter().position(|r| r.message_count >= n).unwrap();
        list.model().borrow().expansions().visible_of(base)
    }

    #[test]
    fn cache_fetches_a_window_around_the_range() {
        let source = source();
        let keys = source
            .thread_keys(
                &ThreadScope::Folder(FakeMailSource::large_folder()),
                &ThreadFilter::default(),
                ListSort::NEWEST_FIRST,
            )
            .unwrap();
        let mut model = ListModel::new(source, DateFormatter::new(UtcOffset::UTC));
        model.keys = ListKeys::Threads(keys);
        model.ensure(500_000..500_020);
        assert_eq!(model.start, 500_000 - OVERSCAN);
        assert_eq!(model.rows.len(), 20 + 2 * OVERSCAN);
        assert!(model.get(500_010).is_some());
        assert!(
            model.get(10).is_none(),
            "nothing outside the window is fetched"
        );
        // A range inside the window doesn't refetch.
        model.ensure(500_001..500_002);
        assert_eq!(model.start, 500_000 - OVERSCAN);
    }

    #[gpui::test]
    fn selection_marks_read_and_navigates_unread(cx: &mut gpui::TestAppContext) {
        let source = source();
        let scope = ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox));
        let (list, cx) = open_list(cx, &source, scope, memory());
        let unread_before = source.unread_count(&FolderKey::Unified(FolderRole::Inbox));
        list.update(cx, |list, cx| {
            assert!(list.len() > 1000);
            assert_eq!(
                list.selected_index(),
                Some(0),
                "opening selects the first thread"
            );
            list.next(cx);
            assert_eq!(list.selected_index(), Some(1));
            list.previous(cx);
            assert_eq!(list.selected_index(), Some(0));
        });

        // `n` walks the unread threads; each one gets marked read when shown.
        let mut visited = Vec::new();
        list.update(cx, |list, cx| {
            while list.jump_unread(Direction::Forward, cx) {
                visited.push(list.selected_index().unwrap());
                if visited.len() == 5 {
                    break;
                }
            }
        });
        assert!(visited.windows(2).all(|w| w[0] < w[1]));
        assert!(source.unread_count(&FolderKey::Unified(FolderRole::Inbox)) < unread_before);

        // `p` goes back to an unread thread, if any is left above.
        list.update(cx, |list, cx| {
            list.toggle_read(cx); // mark the current one unread again
            list.step(5, cx);
            assert!(list.jump_unread(Direction::Backward, cx));
            assert_eq!(list.selected_index(), visited.last().copied());
        });
    }

    #[gpui::test]
    fn archive_removes_the_thread_and_keeps_position(cx: &mut gpui::TestAppContext) {
        let source = source();
        let scope = ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox));
        let (list, cx) = open_list(cx, &source, scope, memory());
        list.update(cx, |list, cx| {
            let len = list.len();
            let first = list.selected_thread().unwrap();
            list.step(3, cx);
            // The first thread is unread again; archiving further down must not
            // pass through it (and mark it read).
            source
                .set_flags(&[first], Flags::empty(), Flags::SEEN)
                .unwrap();
            let next = list.model().borrow_mut().item(4);
            let archived = list.selected_thread().unwrap();
            list.archive_selected(cx);
            assert_eq!(list.len(), len - 1);
            assert_eq!(list.selected_index(), Some(3));
            assert_ne!(list.selected_thread(), Some(archived));
            assert_eq!(list.selected_item(), next, "the next thread moved up");
            assert!(source.thread_rows(&[first])[0].is_unread());
        });
    }

    #[gpui::test]
    fn arrows_expand_enter_and_collapse_a_thread(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            let len = list.len();
            let row = thread_with(list, &source, 3);
            list.select(row, cx);
            let thread = list.selected_thread().unwrap();
            let count = source.thread_rows(&[thread])[0].message_count as usize;

            // →: expand; the reply tree is spliced in after the thread row.
            list.expand_selected(cx);
            assert!(list.is_expanded(row));
            assert_eq!(list.len(), len + count);
            assert_eq!(list.selected_item(), Some(ItemId::Thread(thread)));
            let tree = source.thread_tree(thread).unwrap();
            let children: Vec<ItemId> = (row + 1..=row + count)
                .map(|ix| list.model().borrow_mut().item(ix).unwrap())
                .collect();
            let want: Vec<ItemId> = tree.iter().map(|n| ItemId::Message(n.message)).collect();
            assert_eq!(children, want);
            // The row after the children is the next thread.
            assert!(matches!(
                list.model().borrow_mut().item(row + count + 1),
                Some(ItemId::Thread(_))
            ));

            // → again: into the thread. The first message is selected and shown.
            list.expand_selected(cx);
            assert_eq!(list.selected_index(), Some(row + 1));
            assert_eq!(list.selected_item(), Some(want[0]));
            assert_eq!(list.selected_thread(), Some(thread));
            list.step(1, cx);

            // ←: from a message to its thread, then collapse.
            list.collapse_selected(cx);
            assert_eq!(list.selected_index(), Some(row));
            list.collapse_selected(cx);
            assert!(!list.is_expanded(row));
            assert_eq!(list.len(), len);

            // A single-message thread doesn't expand.
            let single = (0..50)
                .find(|ix| {
                    list.model()
                        .borrow_mut()
                        .row(*ix)
                        .is_some_and(|r| !r.expandable())
                })
                .unwrap();
            list.select(single, cx);
            list.expand_selected(cx);
            assert_eq!(list.len(), len);
        });
    }

    #[gpui::test]
    fn collapsing_a_thread_selects_it_when_a_message_was_selected(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            let row = thread_with(list, &source, 3);
            list.toggle_expanded(row, cx);
            list.select(row + 2, cx);
            assert!(matches!(list.selected_item(), Some(ItemId::Message(_))));
            // Clicking the twisty of the expanded thread.
            list.toggle_expanded(row, cx);
            assert_eq!(list.selected_index(), Some(row));
            assert!(matches!(list.selected_item(), Some(ItemId::Thread(_))));
        });
    }

    #[gpui::test]
    fn expand_all_and_collapse_all(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
                panic!("threaded");
            };
            let counts = source.message_counts(&keys);
            let children: usize = counts.iter().filter(|c| **c > 1).map(|c| *c as usize).sum();
            list.select(5, cx);
            let selected = list.selected_item();

            list.expand_all(cx);
            assert!(!list.is_expanding(), "4,200 threads count synchronously");
            assert_eq!(list.len(), keys.len() + children);
            assert_eq!(list.selected_item(), selected, "the selection stays");

            // Collapsing one thread by hand is remembered as an exception.
            let row = thread_with(list, &source, 2);
            let thread_children = list
                .model()
                .borrow()
                .expansions()
                .children_of(list.model().borrow().expansions().resolve(row).base());
            list.toggle_expanded(row, cx);
            assert_eq!(list.len(), keys.len() + children - thread_children);
            list.refresh(cx);
            assert_eq!(list.len(), keys.len() + children - thread_children);
            assert!(!list.is_expanded(row));

            list.collapse_all(cx);
            assert_eq!(list.len(), keys.len());
            assert_eq!(list.selected_item(), selected);
        });
    }

    #[gpui::test]
    fn expand_all_over_a_million_threads_counts_in_the_background(cx: &mut gpui::TestAppContext) {
        let source = source();
        let scope = ThreadScope::Folder(FakeMailSource::large_folder());
        let (list, cx) = open_list(cx, &source, scope, memory());
        list.update(cx, |list, cx| {
            list.expand_all(cx);
            assert!(list.is_expanding());
            assert_eq!(
                list.len(),
                1_000_000,
                "nothing changes until the counts are in"
            );
        });
        cx.run_until_parked();
        list.update(cx, |list, cx| {
            assert!(!list.is_expanding());
            assert!(list.len() > 1_500_000);
            // Trees are fetched for the rows that are drawn, not for every thread.
            list.model().borrow_mut().ensure(0..60);
            list.edge(true, cx);
            let len = list.len();
            list.model().borrow_mut().ensure(len - 60..len);
            assert!(list.model().borrow().trees.len() < 400);
        });
    }

    #[gpui::test]
    fn expansions_and_selection_survive_live_updates(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            // A thread with replies below the first row.
            let row = (1..200)
                .find(|ix| {
                    list.model().borrow_mut().row(*ix).is_some_and(
                        |r| matches!(r.kind, RowKind::Thread { count, .. } if count >= 3),
                    )
                })
                .unwrap();
            list.toggle_expanded(row, cx);
            list.select(row + 2, cx);
            let selected = list.selected_item();
            let first = list.model().borrow_mut().item(0);
            let Some(ItemId::Thread(first)) = first else {
                panic!("a thread first");
            };
            // Mail moved away above the expanded thread: everything shifts up a row.
            source.archive(&[first]).unwrap();
            list.refresh(cx);
            assert!(list.is_expanded(row - 1));
            assert_eq!(list.selected_index(), Some(row + 1));
            assert_eq!(list.selected_item(), selected);
        });
    }

    #[gpui::test]
    fn threads_off_lists_messages_in_every_folder(cx: &mut gpui::TestAppContext) {
        let source = source();
        let ui_state = memory();
        let (list, cx) = open_list(cx, &source, inbox(), ui_state.clone());
        list.update(cx, |list, cx| {
            let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
                panic!("threaded");
            };
            let messages: u32 = source.message_counts(&keys).iter().sum();
            let thread = list.selected_thread().unwrap();

            list.toggle_threads(cx);
            assert!(!list.threaded());
            assert_eq!(list.key_count(), messages as usize);
            // The same conversation stays selected: its newest message.
            assert!(matches!(list.selected_item(), Some(ItemId::Message(_))));
            assert_eq!(list.selected_thread(), Some(thread));
            // `→` does nothing without threads; `*` neither.
            list.expand_selected(cx);
            list.expand_all(cx);
            assert_eq!(list.len(), messages as usize);

            // Another folder shows the same view, and so does coming back.
            list.open(
                ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox)),
                "Inbox".into(),
                None,
                cx,
            );
            assert!(!list.threaded());
            list.open(inbox(), "Inbox".into(), None, cx);
            assert!(!list.threaded());
            list.toggle_threads(cx);
            assert!(list.threaded());
            assert!(matches!(list.selected_item(), Some(ItemId::Thread(_))));
        });
        assert_eq!(
            ui_state.state().list,
            crate::ui_state::ListState::default(),
            "back to the defaults"
        );
    }

    #[gpui::test]
    fn mode_columns_and_sort_are_saved_to_ui_state(cx: &mut gpui::TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(crate::ui_state::UI_STATE_FILE);
        let source = source();
        let ui_state = UiStateStore::load(&path, ListDefaults::default());
        let (list, cx) = open_list(cx, &source, inbox(), ui_state);
        let newest = list.read_with(cx, |list, _| list.selected_thread());
        list.update_in(cx, |list, window, cx| {
            list.toggle_mode(window, cx);
            list.toggle_column(ColumnId::Size, cx);
            list.set_sort(ListSort::OLDEST_FIRST, cx);
            assert_ne!(
                list.model().borrow_mut().item(0),
                newest.map(ItemId::Thread),
                "oldest first now"
            );
        });
        let reloaded = UiStateStore::load(&path, ListDefaults::default());
        let prefs = reloaded.prefs();
        assert_eq!(prefs.mode, ListMode::Table);
        assert_eq!(prefs.sort, ListSort::OLDEST_FIRST);
        assert!(prefs.columns.contains(&ColumnId::Size));
    }

    /// The subject as the fake sorts it (no `Re:`/`AW:`… prefix, lowercase).
    fn subject_key(subject: &str) -> String {
        let mut rest = subject.trim();
        while let Some((prefix, after)) = rest.split_once(':') {
            if !["re", "fwd", "fw", "aw", "wg"].contains(&prefix.trim().to_lowercase().as_str()) {
                break;
            }
            rest = after.trim_start();
        }
        rest.to_lowercase()
    }

    fn click_header(cx: &mut gpui::VisualTestContext, column: &'static str) {
        let header = cx
            .debug_bounds(column)
            .unwrap_or_else(|| panic!("{column} is drawn"));
        cx.simulate_click(header.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn clicking_a_header_sorts_by_its_column_and_again_reverses(cx: &mut gpui::TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(crate::ui_state::UI_STATE_FILE);
        let source = source();
        let ui_state = UiStateStore::load(&path, ListDefaults::default());
        let mut prefs = ui_state.prefs();
        prefs.mode = ListMode::Table;
        ui_state.set_prefs(&prefs);
        let (list, cx) = open_list(cx, &source, inbox(), ui_state);
        cx.run_until_parked();
        list.update(cx, |list, cx| list.select(7, cx));
        let selected = list.read_with(cx, |list, _| list.selected_item());
        let subjects = |list: &ThreadList| {
            let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
                panic!("threaded");
            };
            source
                .thread_rows(&keys)
                .iter()
                .map(|row| subject_key(row.subject.expose()))
                .collect::<Vec<_>>()
        };

        click_header(cx, "th-subject");
        list.read_with(cx, |list, _| {
            assert_eq!(list.sort(), ListSort::by(SortBy::Subject));
            assert!(!list.sort().descending, "A to Z first");
            let subjects = subjects(list);
            assert_eq!(subjects.len(), 4_200);
            assert!(subjects.windows(2).all(|w| w[0] <= w[1]), "by subject");
            assert_eq!(list.selected_item(), selected, "the selection stays");
        });
        assert_eq!(
            UiStateStore::load(&path, ListDefaults::default())
                .prefs()
                .sort,
            ListSort::by(SortBy::Subject),
            "remembered for every folder"
        );

        click_header(cx, "th-subject");
        list.read_with(cx, |list, _| {
            assert!(list.sort().descending, "again: Z to A");
            assert!(subjects(list).windows(2).all(|w| w[0] >= w[1]));
        });

        click_header(cx, "th-star");
        list.read_with(cx, |list, _| {
            assert_eq!(list.sort(), ListSort::by(SortBy::Star));
            let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
                panic!("threaded");
            };
            let starred: Vec<bool> = source
                .thread_rows(&keys)
                .iter()
                .map(ThreadRow::is_starred)
                .collect();
            assert!(starred[0], "starred first");
            assert!(starred.windows(2).all(|w| w[0] >= w[1]));
        });
        click_header(cx, "th-date");
        assert_eq!(
            list.read_with(cx, |list, _| list.sort()),
            ListSort::NEWEST_FIRST
        );
    }

    /// A store with the table view on, in memory.
    fn table_memory() -> UiStateStore {
        let ui_state = memory();
        let mut prefs = ui_state.prefs();
        prefs.mode = ListMode::Table;
        ui_state.set_prefs(&prefs);
        ui_state
    }

    /// The rows the list shows first, as the source has them.
    fn first_threads(list: &ThreadList, source: &Arc<dyn MailSource>, n: usize) -> Vec<ThreadRow> {
        let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
            panic!("threaded");
        };
        source.thread_rows(&keys[..n])
    }

    fn has_dot(cx: &mut gpui::VisualTestContext, ix: usize) -> bool {
        cx.run_until_parked();
        assert!(
            cx.debug_bounds(Box::leak(format!("unread-{ix}").into_boxed_str()))
                .is_some(),
            "row {ix} has an unread cell"
        );
        cx.debug_bounds(Box::leak(format!("unread-dot-{ix}").into_boxed_str()))
            .is_some()
    }

    fn click_unread_cell(cx: &mut gpui::VisualTestContext, ix: usize) {
        let cell = cx
            .debug_bounds(Box::leak(format!("unread-{ix}").into_boxed_str()))
            .unwrap_or_else(|| panic!("row {ix} has an unread cell"));
        cx.simulate_click(cell.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_unread_column_shows_a_dot_and_a_click_toggles_the_row(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), table_memory());
        cx.run_until_parked();
        assert!(cx.debug_bounds("th-unread").is_some(), "on by default");
        let rows = list.read_with(cx, |list, _| first_threads(list, &source, 12));
        let selected = list.read_with(cx, |list, _| list.selected_item());
        // Not the first row: opening selects it, which marks it read.
        let unread = (1..rows.len()).find(|ix| rows[*ix].is_unread()).unwrap();
        let read = (1..rows.len()).find(|ix| !rows[*ix].is_unread()).unwrap();
        assert!(has_dot(cx, unread), "an unread row has the dot");
        assert!(!has_dot(cx, read), "a read row has none");

        click_unread_cell(cx, unread);
        assert!(!source.thread_rows(&[rows[unread].id])[0].is_unread());
        assert!(!has_dot(cx, unread), "read now");
        assert_eq!(
            list.read_with(cx, |list, _| list.selected_item()),
            selected,
            "the click doesn't select the row"
        );

        click_unread_cell(cx, unread);
        assert!(source.thread_rows(&[rows[unread].id])[0].is_unread());
        assert!(has_dot(cx, unread), "unread again");

        click_unread_cell(cx, read);
        assert!(source.thread_rows(&[rows[read].id])[0].is_unread());
        assert!(has_dot(cx, read));

        // Like `m`, it can be undone.
        list.update(cx, |list, cx| list.undo(cx));
        assert!(!source.thread_rows(&[rows[read].id])[0].is_unread());
        assert!(!has_dot(cx, read));
    }

    #[gpui::test]
    fn a_collapsed_thread_with_one_unread_message_shows_the_dot(cx: &mut gpui::TestAppContext) {
        let source = source();
        let keys = source
            .thread_keys(&inbox(), &ThreadFilter::default(), ListSort::NEWEST_FIRST)
            .unwrap();
        let rows = source.thread_rows(&keys[..12]);
        let ix = (1..rows.len())
            .find(|ix| rows[*ix].message_count >= 3)
            .unwrap();
        let thread = rows[ix].id;
        source
            .set_flags(&[thread], Flags::SEEN, Flags::empty())
            .unwrap();
        let tree = source.thread_tree(thread).unwrap();
        source
            .set_message_flags(&[tree[1].message], Flags::empty(), Flags::SEEN)
            .unwrap();
        assert!(source.thread_rows(&[thread])[0].is_unread());

        let (list, cx) = open_list(cx, &source, inbox(), table_memory());
        cx.run_until_parked();
        list.read_with(cx, |list, _| {
            assert_eq!(list.model().borrow().expansions().count(), 0, "collapsed");
        });
        assert!(has_dot(cx, ix), "one unread message is enough");

        // A click marks the whole thread read.
        click_unread_cell(cx, ix);
        assert!(!has_dot(cx, ix));
        let messages: Vec<MessageId> = tree.iter().map(|node| node.message).collect();
        assert!(
            source
                .message_rows(&messages)
                .iter()
                .all(|row| !row.is_unread())
        );
    }

    #[gpui::test]
    fn a_saved_layout_without_the_unread_column_gets_it(cx: &mut gpui::TestAppContext) {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(crate::ui_state::UI_STATE_FILE);
        std::fs::write(
            &path,
            r#"{ "version": 3, "list": { "view": "table", "columns": ["star", "subject", "size"] } }"#,
        )
        .unwrap();
        let source = source();
        let ui_state = UiStateStore::load(&path, ListDefaults::default());
        let (list, cx) = open_list(cx, &source, inbox(), ui_state);
        cx.run_until_parked();
        assert!(cx.debug_bounds("th-unread").is_some());
        assert!(
            cx.debug_bounds("th-size").is_some(),
            "the other columns stay"
        );
        assert!(cx.debug_bounds("th-correspondents").is_none());
        let star = cx.debug_bounds("th-star").unwrap();
        let unread = cx.debug_bounds("th-unread").unwrap();
        let subject = cx.debug_bounds("th-subject").unwrap();
        assert!(star.left() < unread.left() && unread.left() < subject.left());
        let rows = list.read_with(cx, |list, _| first_threads(list, &source, 12));
        let ix = (1..rows.len()).find(|ix| rows[*ix].is_unread()).unwrap();
        assert!(has_dot(cx, ix));
    }

    #[gpui::test]
    fn threads_off_lists_sort_their_messages_in_the_background(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            list.toggle_threads(cx);
            list.sort_by(SortBy::Size, cx);
            assert!(list.is_loading(), "other orders load in the background");
        });
        cx.run_until_parked();
        list.read_with(cx, |list, _| {
            let ListKeys::Messages(keys) = list.model().borrow().keys().clone() else {
                panic!("threads off");
            };
            let sizes: Vec<u64> = source.message_rows(&keys).iter().map(|r| r.size).collect();
            assert!(sizes.windows(2).all(|w| w[0] >= w[1]), "largest first");
            assert_eq!(list.sort(), ListSort::by(SortBy::Size));
        });
        // `view::SortBy…` keeps the direction; `view::ReverseSortOrder` flips it.
        list.update(cx, |list, cx| {
            list.sort_by(SortBy::Size, cx);
            assert!(list.sort().descending);
            list.reverse_sort(cx);
            assert!(!list.sort().descending);
        });
    }

    #[gpui::test]
    fn archiving_and_switching_folders_in_another_order(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| list.sort_by(SortBy::Subject, cx));
        cx.run_until_parked();
        let next = list.update(cx, |list, cx| {
            list.select(3, cx);
            let next = list.model().borrow_mut().item(4);
            list.archive_selected(cx);
            next
        });
        cx.run_until_parked();
        list.read_with(cx, |list, _| {
            assert_eq!(list.selected_item(), next, "the next row took its place");
            assert_eq!(list.selected_index(), Some(3));
        });

        // Another folder: no rows of the last one while it sorts.
        list.update(cx, |list, cx| {
            list.open(
                ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox)),
                "Inbox".into(),
                None,
                cx,
            );
            assert!(list.is_loading() && list.is_empty());
        });
        cx.run_until_parked();
        list.read_with(cx, |list, _| {
            assert!(list.len() > 4_200);
            assert_eq!(list.selected_index(), Some(0));
        });
    }

    #[gpui::test]
    fn the_message_menu_follows_the_cursor_row(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            let labels = |list: &mut ThreadList| {
                list.message_menu_items()
                    .into_iter()
                    .map(|item| item.map(|(label, action)| (label, action.name())))
                    .collect::<Vec<_>>()
            };
            // An unread row, selected with a right-click: it stays unread.
            let ix = (1..100)
                .find(|ix| {
                    list.model()
                        .borrow_mut()
                        .row(*ix)
                        .is_some_and(|row| row.unread() && !row.starred())
                })
                .unwrap();
            list.context_click(ix, cx);
            assert_eq!(list.selected_index(), Some(ix));
            let items = labels(list);
            assert_eq!(
                items[..4],
                [
                    Some(("Reply".into(), "mail::Reply")),
                    Some(("Reply All".into(), "mail::ReplyAll")),
                    Some(("Forward".into(), "mail::Forward")),
                    None,
                ]
            );
            assert_eq!(items[4], Some(("Mark as Read".into(), "mail::ToggleRead")));
            assert_eq!(items[7], Some(("Report Junk".into(), "mail::ReportJunk")));
            assert_eq!(items[8], Some(("Delete".into(), "mail::Delete")));
            assert_eq!(
                items[9],
                Some(("Move to Folder…".into(), "mail::MoveToFolder"))
            );
            list.toggle_read(cx);
            list.toggle_star(cx);
            let items = labels(list);
            assert_eq!(
                items[4],
                Some(("Mark as Unread".into(), "mail::ToggleRead"))
            );
            assert_eq!(items[5], Some(("Unstar".into(), "mail::ToggleStar")));

            // Inside a multiple selection a right-click keeps it.
            list.select(2, cx);
            list.extend(2, cx);
            list.context_click(3, cx);
            assert_eq!(list.selected_count(), 3);
            list.context_click(9, cx);
            assert_eq!(list.selected_count(), 1);
            assert_eq!(list.selected_index(), Some(9));
        });
    }

    #[gpui::test]
    fn shift_selects_a_range_that_commands_act_on(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            let len = list.len();
            list.select(2, cx);
            list.extend(1, cx);
            list.extend(1, cx);
            assert_eq!(list.selected_index(), Some(4), "the cursor moved");
            assert_eq!(list.selected_count(), 3);
            assert!((2..5).all(|ix| list.is_row_selected(ix)));
            let threads: Vec<ThreadId> = (2..5)
                .map(|ix| match list.model().borrow_mut().item(ix) {
                    Some(ItemId::Thread(id)) => id,
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(
                list.command_items(),
                threads
                    .iter()
                    .copied()
                    .map(ItemId::Thread)
                    .collect::<Vec<_>>()
            );
            // The cursor row decides: star all, then unstar all.
            let starred = source.thread_rows(&threads[2..])[0].is_starred();
            list.toggle_star(cx);
            let rows = source.thread_rows(&threads);
            assert!(rows.iter().all(|r| r.is_starred() != starred), "{rows:?}");
            list.toggle_read(cx);
            let unread = source.thread_rows(&threads)[0].is_unread();
            assert!(
                source
                    .thread_rows(&threads)
                    .iter()
                    .all(|r| r.is_unread() == unread)
            );
            // Extending doesn't mark the rows read; a plain step collapses.
            let next = list.model().borrow_mut().item(5);
            list.archive_selected(cx);
            assert_eq!(list.len(), len - 3);
            assert_eq!(list.selected_count(), 1);
            assert_eq!(list.selected_index(), Some(2));
            assert_eq!(
                list.selected_item(),
                next,
                "the row after the block moved up"
            );
            assert!(
                source.thread_rows(&threads).is_empty(),
                "all three archived"
            );
        });
    }

    #[gpui::test]
    fn cmd_click_and_select_all_survive_live_updates(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            list.select(5, cx);
            list.toggle_row(8, cx);
            list.extend_to(9, cx);
            list.toggle_row(12, cx);
            assert_eq!(list.selected_count(), 4);
            let items: Vec<_> = [5, 8, 9, 12]
                .map(|ix| list.model().borrow_mut().item(ix).unwrap())
                .into();
            assert_eq!(list.command_items(), items);
            // Mail arrives… or rather the top thread leaves: every row moves up one.
            let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
                panic!("threaded");
            };
            source.archive(&[keys[0]]).unwrap();
            list.refresh(cx);
            assert_eq!(list.command_items(), items, "the same rows, by identity");
            assert!([4, 7, 8, 11].iter().all(|ix| list.is_row_selected(*ix)));
            assert_eq!(list.selected_index(), Some(11), "the cursor followed");

            list.select_all(cx);
            assert_eq!(list.selected_count(), list.len());
            source.archive(&[keys[1]]).unwrap();
            list.refresh(cx);
            assert_eq!(list.selected_count(), list.len(), "still everything");
            list.next(cx);
            assert_eq!(list.selected_count(), 1);
        });
    }

    #[gpui::test]
    fn a_mixed_selection_is_one_batch_of_messages(cx: &mut gpui::TestAppContext) {
        use crate::fake::MoveItem;

        let fake = Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
        let source: Arc<dyn MailSource> = fake.clone();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            let row = thread_with(list, &source, 3);
            list.toggle_expanded(row, cx);
            let Some(ItemId::Thread(expanded)) = list.model().borrow_mut().item(row) else {
                panic!("a thread row");
            };
            // The thread row and its first message: the thread covers the message.
            list.select(row, cx);
            list.extend(1, cx);
            assert_eq!(list.command_items(), [ItemId::Thread(expanded)]);
            // A message of the expanded thread and a whole other thread.
            let Some(ItemId::Message(child)) = list.model().borrow_mut().item(row + 2) else {
                panic!("a message row");
            };
            let other_row = if row == 0 { list.len() - 1 } else { 0 };
            let Some(ItemId::Thread(other)) = list.model().borrow_mut().item(other_row) else {
                panic!("a thread row");
            };
            list.select(row + 2, cx);
            list.toggle_row(other_row, cx);
            let items = list.command_items();
            assert_eq!(items.len(), 2);
            let other_messages: Vec<MessageId> = source
                .thread_tree(other)
                .unwrap()
                .iter()
                .map(|n| n.message)
                .collect();
            let trash = FolderId(8);
            list.move_items(&items, trash, None, cx).unwrap();
            assert_eq!(fake.moved_to(MoveItem::Message(child)), Some(trash));
            for message in other_messages {
                assert_eq!(fake.moved_to(MoveItem::Message(message)), Some(trash));
            }
            assert_eq!(fake.moved_to(MoveItem::Thread(other)), None, "one batch");
        });
    }

    #[gpui::test]
    fn select_thread_applies_commands_to_the_whole_thread(cx: &mut gpui::TestAppContext) {
        let source = source();
        let (list, cx) = open_list(cx, &source, inbox(), memory());
        list.update(cx, |list, cx| {
            let row = thread_with(list, &source, 3);
            list.toggle_expanded(row, cx);
            list.select(row + 2, cx);
            let thread = list.selected_thread().unwrap();
            list.select_thread(cx);
            assert_eq!(list.selected_index(), Some(row), "on the thread row");
            assert_eq!(list.selected_whole_thread(), Some(thread));
            let starred = source.thread_rows(&[thread])[0].is_starred();
            list.toggle_star(cx);
            assert_ne!(source.thread_rows(&[thread])[0].is_starred(), starred);
            // Moving on ends the thread selection.
            list.next(cx);
            assert_eq!(list.selected_whole_thread(), None);
        });
    }
}

//! The message list: two-line cards (52 px) or a table (24 px), over a keys-only
//! thread list of any length.
//!
//! The list holds the thread keys of the open folder and a small window of formatted
//! rows around the visible range ([`RowCache`]); rows outside it are never fetched.
//! Cards are a GPUI `uniform_list`, the table is gpui-kit's `DataTable`, and both read
//! the same cache.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, ScrollStrategy, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled, Subscription, Task, UniformListScrollHandle, Window,
    div, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, Size, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    table::{Column, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use mazemail_types::{Flags, ThreadId};
use time::UtcOffset;
use ui::assets::IconName;

use crate::format::{DateFormatter, format_size, format_thousands};
use crate::selection::Selection;
use crate::source::{
    Direction, MailSource, ThreadFilter, ThreadKeys, ThreadPredicate, ThreadRow, ThreadScope,
};

/// Height of a two-line card (CONCEPT §8.2, PLAN §4.9).
pub const CARD_HEIGHT: f32 = 52.;
/// Height of a table row.
pub const TABLE_ROW_HEIGHT: f32 = 24.;
/// Rows fetched beyond the visible range on each side.
const OVERSCAN: usize = 48;
/// Quick Filter typing pause before the list refilters.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(120);

/// How the list draws its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMode {
    /// Two-line cards: sender and date, subject and markers.
    Cards,
    /// Thunderbird's table with columns.
    Table,
}

/// A row, formatted once when fetched so rendering doesn't allocate.
#[derive(Debug, Clone)]
pub struct RowView {
    /// The thread.
    pub id: ThreadId,
    sender: SharedString,
    subject: SharedString,
    date: SharedString,
    size: SharedString,
    count: Option<SharedString>,
    flags: Flags,
    attachment: bool,
}

impl RowView {
    fn new(row: &ThreadRow, dates: &DateFormatter) -> Self {
        Self {
            id: row.id,
            sender: row.sender.expose().clone(),
            subject: row.subject.expose().clone(),
            date: dates.short(row.date),
            size: format_size(row.size),
            count: (row.message_count > 1).then(|| row.message_count.to_string().into()),
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
}

/// Formatted rows for a window of indexes around the visible range.
pub struct RowCache {
    source: Arc<dyn MailSource>,
    keys: ThreadKeys,
    dates: DateFormatter,
    start: usize,
    rows: Vec<Option<RowView>>,
}

impl std::fmt::Debug for RowCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RowCache")
            .field("keys", &self.keys.len())
            .field("start", &self.start)
            .field("rows", &self.rows.len())
            .finish()
    }
}

impl RowCache {
    fn new(source: Arc<dyn MailSource>, dates: DateFormatter) -> Self {
        Self {
            source,
            keys: Arc::from([]),
            dates,
            start: 0,
            rows: Vec::new(),
        }
    }

    fn reset(&mut self, keys: ThreadKeys, dates: DateFormatter) {
        self.keys = keys;
        self.dates = dates;
        self.invalidate();
    }

    fn invalidate(&mut self) {
        self.start = 0;
        self.rows.clear();
    }

    /// Makes sure `range` is cached, fetching it plus overscan if it isn't.
    fn ensure(&mut self, range: Range<usize>) {
        let end = range.end.min(self.keys.len());
        if range.start >= end {
            return;
        }
        if range.start >= self.start && end <= self.start + self.rows.len() {
            return;
        }
        let fetch_start = range.start.saturating_sub(OVERSCAN);
        let fetch_end = (end + OVERSCAN).min(self.keys.len());
        let keys = &self.keys[fetch_start..fetch_end];
        let mut fetched = self.source.thread_rows(keys).into_iter().peekable();
        // Rows come back in key order but may skip threads that disappeared.
        self.rows = keys
            .iter()
            .map(|id| {
                fetched
                    .next_if(|row| row.id == *id)
                    .map(|row| RowView::new(&row, &self.dates))
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

    fn set_flags(&mut self, id: ThreadId, flags: Flags) {
        if let Some(row) = self.rows.iter_mut().flatten().find(|row| row.id == id) {
            row.flags = flags;
        }
    }
}

/// Events from the list.
#[derive(Debug, Clone, PartialEq)]
pub enum ThreadListEvent {
    /// The selected thread changed.
    Selected(Option<ThreadId>),
    /// Flags changed or threads disappeared; unread counts need a refresh.
    Changed,
    /// The list switched between cards and table.
    ModeChanged(ListMode),
}

/// The message list view.
pub struct ThreadList {
    source: Arc<dyn MailSource>,
    offset: UtcOffset,
    scope: ThreadScope,
    title: SharedString,
    search: Option<String>,
    keys: ThreadKeys,
    selection: Selection,
    mode: ListMode,
    cache: Rc<RefCell<RowCache>>,
    scroll: UniformListScrollHandle,
    table: Entity<TableState<ThreadTable>>,
    focus: FocusHandle,
    quick_filter: Entity<InputState>,
    flags_filter: ThreadFilter,
    loading: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for ThreadList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadList").finish_non_exhaustive()
    }
}

impl ThreadList {
    /// A list showing `scope`.
    pub fn new(
        source: Arc<dyn MailSource>,
        offset: UtcOffset,
        scope: ThreadScope,
        title: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let dates = DateFormatter::new(offset);
        let cache = Rc::new(RefCell::new(RowCache::new(source.clone(), dates)));
        let table = cx.new(|cx| {
            TableState::new(
                ThreadTable {
                    cache: cache.clone(),
                },
                window,
                cx,
            )
            .col_movable(false)
            .sortable(false)
        });
        let quick_filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Quick filter…  ⌘⇧K")
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
            keys: Arc::from([]),
            selection: Selection::default(),
            mode: ListMode::Cards,
            cache,
            scroll: UniformListScrollHandle::new(),
            table,
            focus: cx.focus_handle(),
            quick_filter,
            flags_filter: ThreadFilter::default(),
            loading: None,
            _subscriptions: subscriptions,
        };
        this.open(scope, title, None, cx);
        this
    }

    /// Opens a folder (or search results) and selects its first thread.
    pub fn open(
        &mut self,
        scope: ThreadScope,
        title: SharedString,
        search: Option<String>,
        cx: &mut Context<'_, Self>,
    ) {
        self.scope = scope;
        self.title = title;
        self.search = search;
        self.reload(true, cx);
    }

    /// The open scope.
    pub fn scope(&self) -> &ThreadScope {
        &self.scope
    }

    /// Title of the open folder.
    pub fn title(&self) -> &SharedString {
        &self.title
    }

    /// Number of threads shown.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Whether keys are still loading (text filters run in the background).
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// The selected index.
    pub fn selected_index(&self) -> Option<usize> {
        self.selection.selected()
    }

    /// The selected thread.
    pub fn selected_thread(&self) -> Option<ThreadId> {
        self.selection
            .selected()
            .and_then(|ix| self.keys.get(ix).copied())
    }

    /// The current display mode.
    pub fn mode(&self) -> ListMode {
        self.mode
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

    /// Reloads the keys for the current scope and filter. Unfiltered folders load
    /// synchronously (cheap key lists); text filters load in the background.
    fn reload(&mut self, select_first: bool, cx: &mut Context<'_, Self>) {
        let filter = self.filter(cx);
        let scope = self.scope.clone();
        if filter.text.is_none() {
            let started = std::time::Instant::now();
            let keys = self.source.thread_keys(&scope, &filter);
            tracing::debug!(
                elapsed_ms = started.elapsed().as_secs_f64() * 1e3,
                "folder keys loaded"
            );
            self.loading = None;
            self.set_keys(keys.unwrap_or_else(|_| Arc::from([])), select_first, cx);
            return;
        }
        let source = self.source.clone();
        let task = cx.background_spawn(async move { source.thread_keys(&scope, &filter) });
        self.loading = Some(cx.spawn(async move |this, cx| {
            let keys = task.await;
            this.update(cx, |this, cx| {
                this.loading = None;
                this.set_keys(keys.unwrap_or_else(|_| Arc::from([])), select_first, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn set_keys(&mut self, keys: ThreadKeys, select_first: bool, cx: &mut Context<'_, Self>) {
        let previous = self.selected_thread();
        self.cache
            .borrow_mut()
            .reset(keys.clone(), DateFormatter::new(self.offset));
        self.keys = keys;
        let keep = previous.and_then(|id| self.keys.iter().position(|k| *k == id));
        self.selection.reset(self.keys.len(), keep);
        if select_first || self.selection.selected().is_none() {
            self.selection.first();
        }
        self.table.update(cx, |table, cx| table.refresh(cx));
        self.reveal_selection(ScrollStrategy::Top, cx);
        self.selection_changed(cx);
    }

    /// Selects `ix`, marks it read and tells the window.
    pub fn select(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if self.selection.selected() == Some(ix) {
            return;
        }
        self.selection.select(ix);
        self.reveal_selection(ScrollStrategy::Nearest, cx);
        self.selection_changed(cx);
    }

    fn selection_changed(&mut self, cx: &mut Context<'_, Self>) {
        let thread = self.selected_thread();
        if let Some(id) = thread {
            self.mark_read(id, cx);
        }
        if let (ListMode::Table, Some(ix)) = (self.mode, self.selection.selected()) {
            self.table.update(cx, |table, cx| {
                if table.selected_row() != Some(ix) {
                    table.set_selected_row(ix, cx);
                }
            });
        }
        cx.emit(ThreadListEvent::Selected(thread));
        cx.notify();
    }

    fn reveal_selection(&mut self, strategy: ScrollStrategy, cx: &mut Context<'_, Self>) {
        let Some(ix) = self.selection.selected() else {
            return;
        };
        match self.mode {
            ListMode::Cards => self.scroll.scroll_to_item(ix, strategy),
            ListMode::Table => self
                .table
                .update(cx, |table, cx| table.scroll_to_row(ix, cx)),
        }
    }

    /// Marks a thread read when it's displayed, as Thunderbird does.
    fn mark_read(&mut self, id: ThreadId, cx: &mut Context<'_, Self>) {
        let unread = self
            .cache
            .borrow_mut()
            .row(self.selection.selected().unwrap_or(0))
            .filter(|row| row.id == id)
            .is_some_and(RowView::unread);
        if unread {
            self.change_flags(id, Flags::SEEN, Flags::empty(), cx);
        }
    }

    fn change_flags(
        &mut self,
        id: ThreadId,
        add: Flags,
        remove: Flags,
        cx: &mut Context<'_, Self>,
    ) {
        match self.source.set_flags(&[id], add, remove) {
            Ok(()) => {
                if let Some(row) = self.source.thread_rows(&[id]).first() {
                    self.cache.borrow_mut().set_flags(id, row.flags);
                }
                cx.emit(ThreadListEvent::Changed);
                cx.notify();
            }
            Err(error) => tracing::warn!(%error, "changing flags failed"),
        }
    }

    fn selected_flags(&mut self) -> Option<(ThreadId, Flags)> {
        let ix = self.selection.selected()?;
        let mut cache = self.cache.borrow_mut();
        cache.row(ix).map(|row| (row.id, row.flags))
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
            self.reveal_selection(ScrollStrategy::Nearest, cx);
            self.selection_changed(cx);
        }
    }

    /// Moves by a page of rows.
    pub fn page(&mut self, down: bool, cx: &mut Context<'_, Self>) {
        let row_height = match self.mode {
            ListMode::Cards => CARD_HEIGHT,
            ListMode::Table => TABLE_ROW_HEIGHT,
        };
        let viewport = f32::from(self.scroll.0.borrow().base_handle.bounds().size.height);
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
            self.reveal_selection(ScrollStrategy::Nearest, cx);
            self.selection_changed(cx);
        }
    }

    /// `n` / `p`: next or previous unread thread; stays put if there is none.
    pub fn jump_unread(&mut self, direction: Direction, cx: &mut Context<'_, Self>) -> bool {
        let source = self.source.clone();
        let keys = self.keys.clone();
        let moved = self.selection.jump(direction, |from, direction| {
            source.find(&keys, from, direction, ThreadPredicate::Unread)
        });
        if moved {
            self.reveal_selection(ScrollStrategy::Center, cx);
            self.selection_changed(cx);
        }
        moved
    }

    /// `m`: toggles read/unread of the selected thread.
    pub fn toggle_read(&mut self, cx: &mut Context<'_, Self>) {
        if let Some((id, flags)) = self.selected_flags() {
            if flags.contains(Flags::SEEN) {
                self.change_flags(id, Flags::empty(), Flags::SEEN, cx);
            } else {
                self.change_flags(id, Flags::SEEN, Flags::empty(), cx);
            }
        }
    }

    /// `s`: toggles the star of the selected thread.
    pub fn toggle_star(&mut self, cx: &mut Context<'_, Self>) {
        if let Some((id, flags)) = self.selected_flags() {
            if flags.contains(Flags::FLAGGED) {
                self.change_flags(id, Flags::empty(), Flags::FLAGGED, cx);
            } else {
                self.change_flags(id, Flags::FLAGGED, Flags::empty(), cx);
            }
        }
    }

    /// `a`: archives the selected thread and selects the next one.
    pub fn archive_selected(&mut self, cx: &mut Context<'_, Self>) {
        let Some(id) = self.selected_thread() else {
            return;
        };
        match self.source.archive(&[id]) {
            Ok(()) => {
                let ix = self.selection.selected().unwrap_or(0);
                self.reload(false, cx);
                // The next thread moved into the archived one's place.
                self.selection.reset(
                    self.keys.len(),
                    Some(ix.min(self.keys.len().saturating_sub(1))),
                );
                self.selection_changed(cx);
                cx.emit(ThreadListEvent::Changed);
            }
            Err(error) => tracing::warn!(%error, "archiving failed"),
        }
    }

    /// Switches between cards and table.
    pub fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.mode = match self.mode {
            ListMode::Cards => ListMode::Table,
            ListMode::Table => ListMode::Cards,
        };
        if let (ListMode::Table, Some(ix)) = (self.mode, self.selection.selected()) {
            self.table
                .update(cx, |table, cx| table.set_selected_row(ix, cx));
        }
        self.reveal_selection(ScrollStrategy::Center, cx);
        self.focus(window, cx);
        cx.emit(ThreadListEvent::ModeChanged(self.mode));
        cx.notify();
    }

    /// Focuses the list so single-key shortcuts work.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        match self.mode {
            ListMode::Cards => window.focus(&self.focus, cx),
            ListMode::Table => window.focus(&self.table.focus_handle(cx), cx),
        }
    }

    /// Focuses the Quick Filter (`cmd-shift-k`).
    pub fn focus_quick_filter(&self, window: &mut Window, cx: &mut App) {
        let handle = self.quick_filter.focus_handle(cx);
        window.focus(&handle, cx);
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
        if let TableEvent::SelectRow(ix) = event {
            self.select(*ix, cx);
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

    fn render_card(
        &self,
        ix: usize,
        row: Option<&RowView>,
        cx: &mut Context<'_, Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().colors;
        let selected = self.selection.selected() == Some(ix);
        let base = div()
            .id(ix)
            .h(px(CARD_HEIGHT))
            .w_full()
            .flex()
            .flex_col()
            .justify_center()
            .gap_0p5()
            .relative()
            .pl_2()
            .pr_3()
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .map(|this| {
                if selected {
                    this.bg(theme.list_active).child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .bottom_0()
                            .w(px(2.))
                            .bg(theme.primary),
                    )
                } else {
                    this.hover(|this| this.bg(theme.list_hover))
                }
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.focus(window, cx);
                this.select(ix, cx);
            }));
        let Some(row) = row else {
            return base;
        };
        let unread = row.unread();
        base.child(
            h_flex()
                .gap_1p5()
                .child(
                    div()
                        .size(px(7.))
                        .flex_none()
                        .rounded_full()
                        .when(unread, |this| this.bg(theme.primary)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .when(unread, |this| this.font_semibold())
                        .child(row.sender.clone()),
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
        )
        .child(
            h_flex()
                .gap_1p5()
                .pl(px(13.))
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

    fn render_cards(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Stateful<Div>> {
        self.cache.borrow_mut().ensure(range.clone());
        let cache = self.cache.clone();
        let cache = cache.borrow();
        let mut cards = Vec::with_capacity(range.len());
        for ix in range {
            cards.push(self.render_card(ix, cache.get(ix), cx));
        }
        cards
    }

    fn render_header(&self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let count = if self.loading.is_some() && self.filter(cx).text.is_some() {
            SharedString::from("Searching…")
        } else {
            format!("{} threads", format_thousands(self.keys.len() as u64)).into()
        };
        let filter = self.flags_filter.clone();
        v_flex()
            .flex_none()
            .gap_1p5()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
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
                            .text_color(theme.muted_foreground)
                            .child(count),
                    )
                    .child(
                        Button::new("toggle-table-view")
                            .ghost()
                            .xsmall()
                            .icon(match self.mode {
                                ListMode::Cards => IconName::Table,
                                ListMode::Table => IconName::Rows3,
                            })
                            .tooltip(match self.mode {
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

impl EventEmitter<ThreadListEvent> for ThreadList {}

impl Focusable for ThreadList {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match self.mode {
            ListMode::Cards => self.focus.clone(),
            ListMode::Table => self.table.focus_handle(cx),
        }
    }
}

impl Render for ThreadList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let empty = self.keys.is_empty() && self.loading.is_none();
        let body = match self.mode {
            ListMode::Cards => div()
                .id("thread-cards")
                .size_full()
                .track_focus(&self.focus)
                .child(
                    uniform_list(
                        "threads",
                        self.keys.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            this.render_cards(range, cx)
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.scroll),
                )
                .into_any_element(),
            ListMode::Table => div()
                .size_full()
                .child(
                    DataTable::new(&self.table)
                        .with_size(Size::Size(px(TABLE_ROW_HEIGHT)))
                        .stripe(false)
                        .bordered(false),
                )
                .into_any_element(),
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
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("No messages"),
                        )
                    })
                    .when(!empty, |this| this.child(body)),
            )
    }
}

/// The table view's delegate: columns and cells over the shared [`RowCache`].
pub struct ThreadTable {
    cache: Rc<RefCell<RowCache>>,
}

impl std::fmt::Debug for ThreadTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadTable").finish_non_exhaustive()
    }
}

/// Table columns, Thunderbird's defaults.
const COLUMNS: &[(&str, &str, f32)] = &[
    ("star", "", 26.),
    ("attachment", "", 26.),
    ("subject", "Subject", 250.),
    ("sender", "Correspondent", 150.),
    ("date", "Date", 84.),
    ("size", "Size", 64.),
];

/// The list width the table needs to show every column.
pub const TABLE_LIST_WIDTH: f32 = 640.;

impl TableDelegate for ThreadTable {
    fn columns_count(&self, _: &App) -> usize {
        COLUMNS.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.cache.borrow().keys.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let (key, name, width) = COLUMNS[col_ix];
        let column = Column::new(key, name).width(px(width));
        match key {
            "star" | "attachment" => column.resizable(false).text_center(),
            "size" => column.text_right(),
            _ => column,
        }
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _: &mut Window,
        _: &mut Context<'_, TableState<Self>>,
    ) {
        self.cache.borrow_mut().ensure(visible_range);
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let (key, name, _) = COLUMNS[col_ix];
        let muted = cx.theme().muted_foreground;
        match key {
            "star" => div().child(Icon::new(IconName::Star).xsmall().text_color(muted)),
            "attachment" => div().child(Icon::new(IconName::Paperclip).xsmall().text_color(muted)),
            _ => div().text_xs().child(name),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<'_, TableState<Self>>,
    ) -> impl IntoElement {
        let mut cache = self.cache.borrow_mut();
        let Some(row) = cache.row(row_ix) else {
            return div();
        };
        let theme = cx.theme();
        let unread = row.unread();
        let cell = div()
            .text_xs()
            .truncate()
            .when(unread, |this| this.font_semibold());
        match COLUMNS[col_ix].0 {
            "star" => cell.when(row.starred(), |this| {
                this.child(
                    Icon::new(IconName::StarFill)
                        .xsmall()
                        .text_color(theme.warning),
                )
            }),
            "attachment" => cell.when(row.attachment, |this| {
                this.child(
                    Icon::new(IconName::Paperclip)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
            }),
            "subject" => cell.child(row.subject.clone()),
            "sender" => cell.child(row.sender.clone()),
            "date" => cell
                .text_color(theme.muted_foreground)
                .child(row.date.clone()),
            _ => cell
                .text_color(theme.muted_foreground)
                .child(row.size.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use mazemail_types::FolderRole;
    use time::macros::datetime;

    use super::*;
    use crate::fake::FakeMailSource;
    use crate::source::FolderKey;

    fn source() -> Arc<dyn MailSource> {
        Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)))
    }

    #[test]
    fn cache_fetches_a_window_around_the_range() {
        let source = source();
        let keys = source
            .thread_keys(
                &ThreadScope::Folder(FakeMailSource::large_folder()),
                &ThreadFilter::default(),
            )
            .unwrap();
        let mut cache = RowCache::new(source, DateFormatter::new(UtcOffset::UTC));
        cache.reset(keys, DateFormatter::new(UtcOffset::UTC));
        cache.ensure(500_000..500_020);
        assert_eq!(cache.start, 500_000 - OVERSCAN);
        assert_eq!(cache.rows.len(), 20 + 2 * OVERSCAN);
        assert!(cache.get(500_010).is_some());
        assert!(
            cache.get(10).is_none(),
            "nothing outside the window is fetched"
        );
        // A range inside the window doesn't refetch.
        cache.ensure(500_001..500_002);
        assert_eq!(cache.start, 500_000 - OVERSCAN);
    }

    #[gpui::test]
    fn selection_marks_read_and_navigates_unread(cx: &mut gpui::TestAppContext) {
        cx.update(ui::init);
        let source = source();
        let (list, cx) = cx.add_window_view(|window, cx| {
            ThreadList::new(
                source.clone(),
                UtcOffset::UTC,
                ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox)),
                "Inbox".into(),
                window,
                cx,
            )
        });
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
        cx.update(ui::init);
        let source = source();
        let (list, cx) = cx.add_window_view(|window, cx| {
            ThreadList::new(
                source.clone(),
                UtcOffset::UTC,
                ThreadScope::Folder(FolderKey::Unified(FolderRole::Inbox)),
                "Inbox".into(),
                window,
                cx,
            )
        });
        list.update(cx, |list, cx| {
            let len = list.len();
            list.step(3, cx);
            let archived = list.selected_thread().unwrap();
            list.archive_selected(cx);
            assert_eq!(list.len(), len - 1);
            assert_eq!(list.selected_index(), Some(3));
            assert_ne!(list.selected_thread(), Some(archived));
        });
    }
}

//! The Maze main window: title bar, spaces bar, folder pane, message list, message
//! view, agent column and status bar (CONCEPT §8.2), plus the command palette and the
//! Quick Move picker.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyView, AnyWindowHandle, App, AppContext as _, Axis, Context, DragMoveEvent, Empty, Entity,
    FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement, PromptLevel, Render,
    SharedString, Styled, Subscription, Window, WindowBounds, WindowOptions, canvas, div, point,
    prelude::FluentBuilder as _, px, size,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, TitleBar, WindowExt as _, h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    resizable::{HandleEdge, resize_handle},
    v_flex,
};
use mazemail_types::{FolderRole, PRODUCT_NAME};
use time::UtcOffset;
use ui::assets::IconName;

use crate::actions::*;
use crate::chrome::{AgentColumn, MailStatusBar, SpacesBar};
use crate::command_palette::{CommandPalette, Dismissed};
use crate::folder_pane::{FolderPane, FolderPaneEvent};
use crate::format::format_thousands;
use crate::layout::{self, AgentPlacement, LayoutConfig, LayoutPrefs, MessagePlacement};
use crate::message_view::MessageView;
use crate::perf::{FrameStats, ScrollBenchmark, resident_memory};
use crate::quick_move::{MoveTarget, QuickMove, QuickMoveEvent, source_folder};
use crate::source::{DataChanges, Direction, FolderKey, MailSource, ThreadScope};
use crate::thread_list::{
    CARD_HEIGHT, ItemId, ListMode, TABLE_LIST_WIDTH, TABLE_ROW_HEIGHT, ThreadList, ThreadListEvent,
};
use crate::ui_state::{ListDefaults, UiStateStore};

/// How the main window starts.
#[derive(Debug, Clone)]
pub struct MailWindowOptions {
    /// The user's UTC offset, for dates. Read it before threads start (`time` can't
    /// read it safely later on Unix).
    pub local_offset: UtcOffset,
    /// When the process started, for the startup measurement.
    pub process_start: Instant,
    /// Run the scripted scroll benchmark after the first frame.
    pub benchmark: Option<ScrollBenchmark>,
    /// Initial window size; by default 1680×1000, shrunk to fit the screen.
    pub window_size: Option<(f32, f32)>,
    /// The message list view (`ui_state.json`); in memory by default.
    pub ui_state: UiStateStore,
}

impl Default for MailWindowOptions {
    fn default() -> Self {
        Self {
            local_offset: UtcOffset::UTC,
            process_start: Instant::now(),
            benchmark: None,
            window_size: None,
            ui_state: UiStateStore::in_memory(ListDefaults::default()),
        }
    }
}

/// Default window size; macOS shrinks it to fit smaller screens.
const DEFAULT_SIZE: (f32, f32) = (1680., 1000.);
/// The window can't get smaller than this.
const MIN_SIZE: (f32, f32) = (720., 480.);
/// Selected rows whose account Quick Move checks before it opens.
const ACCOUNT_CHECK_ROWS: usize = 1_000;

/// Opens the main window. Returns its handle and root view (for
/// [`MailWindow::apply_changes`]).
pub fn open_main_window(
    source: Arc<dyn MailSource>,
    options: MailWindowOptions,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<MailWindow>), OpenWindowError> {
    let (width, height) = options.window_size.unwrap_or_else(|| {
        // Leave a margin around the window on screens smaller than the default.
        let screen = cx.primary_display().map(|display| display.bounds().size);
        screen.map_or(DEFAULT_SIZE, |screen| {
            (
                DEFAULT_SIZE.0.min(f32::from(screen.width) * 0.94),
                DEFAULT_SIZE.1.min(f32::from(screen.height) * 0.9),
            )
        })
    });
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(size(px(width), px(height)), cx)),
        window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(PRODUCT_NAME.into()),
            ..TitleBar::title_bar_options()
        }),
        app_id: Some(mazemail_types::TECHNICAL_NAME.to_owned()),
        ..TitleBar::window_options()
    };
    gpui_kit::open_window(window_options, cx, move |window, cx| {
        cx.new(|cx| MailWindow::new(source, &options, window, cx))
    })
    .map_err(|error| OpenWindowError(error.to_string()))
}

/// The main window couldn't be opened.
#[derive(Debug, thiserror::Error)]
#[error("could not open the main window: {0}")]
pub struct OpenWindowError(pub String);

/// A column being resized; also the drag payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnDrag {
    FolderPane,
    List,
    Agent,
}

impl Render for ColumnDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        Empty
    }
}

/// A benchmark in progress.
#[derive(Debug)]
struct BenchRun {
    config: ScrollBenchmark,
    frame: usize,
}

/// The main window's root view.
pub struct MailWindow {
    source: Arc<dyn MailSource>,
    focus: FocusHandle,
    folder_pane: Entity<FolderPane>,
    thread_list: Entity<ThreadList>,
    message_view: Entity<MessageView>,
    search: Entity<InputState>,
    prefs: LayoutPrefs,
    config: LayoutConfig,
    palette: Option<Entity<CommandPalette>>,
    palette_return_focus: Option<FocusHandle>,
    quick_move: Option<Entity<QuickMove>>,
    quick_move_return_focus: Option<FocusHandle>,
    ui_state: UiStateStore,
    cards_list_width: Option<f32>,
    stats: Rc<RefCell<FrameStats>>,
    pending_benchmark: Option<ScrollBenchmark>,
    bench: Option<BenchRun>,
    title_bar_items: Vec<AnyView>,
    after_paint: Rc<RefCell<Vec<AfterPaint>>>,
    _subscriptions: Vec<Subscription>,
}

/// A callback for after the next frame was painted.
type AfterPaint = Box<dyn FnOnce(&mut App)>;

impl std::fmt::Debug for MailWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailWindow").finish_non_exhaustive()
    }
}

impl MailWindow {
    /// Builds the window content.
    pub fn new(
        source: Arc<dyn MailSource>,
        options: &MailWindowOptions,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let initial = FolderKey::Unified(FolderRole::Inbox);
        let folder_pane = cx.new(|cx| FolderPane::new(source.clone(), &initial, window, cx));
        let title = folder_pane
            .read(cx)
            .title_of(&initial)
            .unwrap_or_else(|| "Inbox".into());
        let message_view = cx.new(|cx| MessageView::new(options.local_offset, cx));
        let thread_list = cx.new(|cx| {
            ThreadList::new(
                source.clone(),
                options.local_offset,
                ThreadScope::Folder(initial),
                title,
                options.ui_state.clone(),
                window,
                cx,
            )
        });
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search all messages…  ⌘K")
                .clean_on_escape()
        });
        let subscriptions = vec![
            cx.subscribe_in(&folder_pane, window, Self::on_folder_pane_event),
            cx.subscribe_in(&thread_list, window, Self::on_thread_list_event),
            cx.subscribe_in(&search, window, Self::on_search_event),
            ui::theme::observe_system_appearance(window),
        ];
        let mut this = Self {
            source,
            focus: cx.focus_handle(),
            folder_pane,
            thread_list,
            message_view,
            search,
            prefs: LayoutPrefs::default(),
            config: LayoutConfig::default(),
            palette: None,
            palette_return_focus: None,
            quick_move: None,
            quick_move_return_focus: None,
            ui_state: options.ui_state.clone(),
            cards_list_width: None,
            stats: Rc::new(RefCell::new(FrameStats::new(options.process_start))),
            pending_benchmark: options.benchmark,
            bench: None,
            title_bar_items: Vec::new(),
            after_paint: Rc::default(),
            _subscriptions: subscriptions,
        };
        if this.thread_list.read(cx).mode() == ListMode::Table {
            // The first folder remembered the table view.
            this.fit_list_to_table(cx);
        }
        this.show_selected(cx);
        let focus = this.thread_list.focus_handle(cx);
        window.focus(&focus, cx);
        this
    }

    /// The message list.
    pub fn thread_list(&self) -> &Entity<ThreadList> {
        &self.thread_list
    }

    /// The message view.
    pub fn message_view(&self) -> &Entity<MessageView> {
        &self.message_view
    }

    /// The folder pane.
    pub fn folder_pane(&self) -> &Entity<FolderPane> {
        &self.folder_pane
    }

    /// The layout preferences (column widths and toggles).
    pub fn prefs(&self) -> &LayoutPrefs {
        &self.prefs
    }

    /// Whether the command palette is open.
    pub fn is_palette_open(&self) -> bool {
        self.palette.is_some()
    }

    /// The Quick Move picker, while it is open.
    pub fn quick_move(&self) -> Option<&Entity<QuickMove>> {
        self.quick_move.as_ref()
    }

    /// Adds a view to the right of the title bar, before the search field (the
    /// auto-updater's "Restart to update" button). Crates above `mail_ui` plug in here,
    /// so the window doesn't depend on them.
    pub fn add_title_bar_item(&mut self, view: AnyView, cx: &mut Context<'_, Self>) {
        self.title_bar_items.push(view);
        cx.notify();
    }

    /// Runs `callback` (deferred, outside of painting) after the next frame has been
    /// painted, and requests that frame. The crash guard uses it to learn that the
    /// window really rendered.
    pub fn after_next_paint(
        &mut self,
        callback: impl FnOnce(&mut App) + 'static,
        cx: &mut Context<'_, Self>,
    ) {
        self.after_paint.borrow_mut().push(Box::new(callback));
        cx.notify();
    }

    /// Re-reads what `changes` affects: the folder tree, the counts, the open list's
    /// keys (keeping the selection) and the displayed message.
    pub fn apply_changes(&mut self, changes: &DataChanges, cx: &mut Context<'_, Self>) {
        if changes.is_empty() {
            return;
        }
        if changes.everything || changes.folders {
            self.folder_pane.update(cx, |pane, cx| pane.reload(cx));
        } else if changes.counts {
            self.folder_pane
                .update(cx, |pane, cx| pane.refresh_counts(cx));
        }
        let before = self.thread_list.read(cx).selected_item();
        if changes.everything || changes.threads || !changes.threads_updated.is_empty() {
            self.thread_list.update(cx, |list, cx| list.refresh(cx));
        }
        // A changed selection was shown through the list's event already.
        let list = self.thread_list.read(cx);
        let selected = list.selected_item();
        let selected_thread = list.selected_thread();
        let showing_doc = self.message_view.read(cx).document();
        let showing_selected = match (selected, showing_doc) {
            (Some(ItemId::Thread(thread)), Some(doc)) => doc.thread == thread,
            (Some(ItemId::Message(message)), Some(doc)) => doc.message == message,
            (None, None) => true,
            _ => false,
        };
        let message_stale =
            showing_doc.is_some_and(|doc| changes.messages_updated.contains(&doc.message));
        let stale = message_stale
            || selected_thread.is_some_and(|id| changes.threads_updated.contains(&id));
        if selected == before && (changes.everything || stale || !showing_selected) {
            self.show_selected(cx);
        }
        cx.notify();
    }

    fn show_selected(&mut self, cx: &mut Context<'_, Self>) {
        let count = self.thread_list.read(cx).selected_count();
        if count > 1 {
            self.message_view
                .update(cx, |view, cx| view.show_selection(count, cx));
            return;
        }
        let item = self.thread_list.read(cx).selected_item();
        let document = item.map(|item| match item {
            ItemId::Thread(thread) => self.source.message(thread),
            ItemId::Message(message) => self.source.message_document(message),
        });
        self.message_view.update(cx, |view, cx| match document {
            None => view.show(None, cx),
            Some(Ok(document)) => view.show(Some(document), cx),
            Some(Err(error)) => view.show_error(error.to_string(), cx),
        });
    }

    fn on_folder_pane_event(
        &mut self,
        _: &Entity<FolderPane>,
        event: &FolderPaneEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let FolderPaneEvent::Selected { key, title } = event;
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.thread_list.update(cx, |list, cx| {
            list.open(ThreadScope::Folder(key.clone()), title.clone(), None, cx)
        });
    }

    fn on_thread_list_event(
        &mut self,
        _: &Entity<ThreadList>,
        event: &ThreadListEvent,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            ThreadListEvent::Selected(_) => self.show_selected(cx),
            ThreadListEvent::Changed => {
                self.folder_pane
                    .update(cx, |pane, cx| pane.refresh_counts(cx));
            }
            ThreadListEvent::ModeChanged(ListMode::Table) => self.fit_list_to_table(cx),
            ThreadListEvent::ModeChanged(ListMode::Cards) => {
                if let Some(width) = self.cards_list_width.take() {
                    self.prefs.list = width;
                }
            }
            ThreadListEvent::ColumnsChanged => {
                if self.thread_list.read(cx).mode() == ListMode::Table {
                    let needed = self.thread_list.read(cx).table_width();
                    self.prefs.list = self.prefs.list.max(needed);
                }
            }
        }
        cx.notify();
    }

    /// The table needs room for its columns; remembers the card width.
    fn fit_list_to_table(&mut self, cx: &App) {
        self.cards_list_width = Some(self.prefs.list);
        let needed = self.thread_list.read(cx).table_width();
        self.prefs.list = self.prefs.list.max(needed.max(TABLE_LIST_WIDTH));
    }

    fn on_search_event(
        &mut self,
        search: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let InputEvent::PressEnter { .. } = event {
            let query = search.read(cx).value().trim().to_owned();
            if query.is_empty() {
                return;
            }
            let title: SharedString = format!("Search: “{query}”").into();
            self.thread_list.update(cx, |list, cx| {
                list.open(ThreadScope::AllFolders, title, Some(query), cx);
                list.focus(window, cx);
            });
        }
    }

    fn effective_agent_expanded(&self, window: &Window) -> bool {
        let width = f32::from(window.viewport_size().width);
        layout::agent_expanded(width, &self.prefs, &self.config)
    }

    fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.palette.is_some() {
            self.close_palette(window, cx);
            return;
        }
        self.palette_return_focus = window.focused(cx);
        let return_focus = self.palette_return_focus.clone();
        let palette = cx.new(|cx| CommandPalette::new(return_focus.as_ref(), window, cx));
        let focus = palette.focus_handle(cx);
        self._subscriptions.push(cx.subscribe_in(
            &palette,
            window,
            |this, _, _: &Dismissed, window, cx| {
                this.close_palette(window, cx);
            },
        ));
        self.palette = Some(palette);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.palette.take().is_none() {
            return;
        }
        match self.palette_return_focus.take() {
            Some(focus) => window.focus(&focus, cx),
            None => {
                let focus = self.thread_list.focus_handle(cx);
                window.focus(&focus, cx);
            }
        }
        cx.notify();
    }

    /// `shift-m`: opens Quick Move for the selected rows (or thread) over their
    /// account's folders. Run from the palette, it takes the palette's place.
    fn open_quick_move(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.quick_move.is_some() {
            return;
        }
        let items = self.thread_list.update(cx, |list, _| list.command_items());
        // Mail moves within its account. The first rows tell (checking a huge selection
        // row by row would stall; the core refuses a move across accounts anyway).
        let mut accounts = items
            .iter()
            .take(ACCOUNT_CHECK_ROWS)
            .map(|item| match item {
                ItemId::Thread(thread) => self.source.thread_account(*thread),
                ItemId::Message(message) => self.source.message_account(*message),
            });
        let Some(Some(account)) = accounts.next() else {
            return;
        };
        if !accounts.all(|other| other == Some(account)) {
            window.push_notification(
                Notification::info("Quick Move works on mail of one account at a time.")
                    .title(PRODUCT_NAME),
                cx,
            );
            return;
        }
        let tree = self.source.folder_tree();
        let Some(account) = tree.accounts.into_iter().find(|a| a.id == account) else {
            tracing::warn!("quick move: the selected mail's account is gone");
            return;
        };
        let from = source_folder(self.thread_list.read(cx).scope(), &account);
        let target = MoveTarget {
            items,
            account: account.id,
            from,
        };
        let recent = self.ui_state.move_targets(account.id);
        let return_focus = match self.palette.take() {
            Some(_) => self.palette_return_focus.take(),
            None => window.focused(cx),
        };
        self.quick_move_return_focus = return_focus;
        let picker = cx.new(|cx| QuickMove::new(target, &account, recent, window, cx));
        let focus = picker.focus_handle(cx);
        self._subscriptions
            .push(cx.subscribe_in(&picker, window, Self::on_quick_move_event));
        self.quick_move = Some(picker);
        window.focus(&focus, cx);
        cx.notify();
    }

    fn on_quick_move_event(
        &mut self,
        picker: &Entity<QuickMove>,
        event: &QuickMoveEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let QuickMoveEvent::Confirmed(folder) = *event {
            let target = picker.read(cx).target().clone();
            let moved = self.thread_list.update(cx, |list, cx| {
                list.move_items(&target.items, folder, target.from, cx)
            });
            match moved {
                Ok(()) => self.ui_state.remember_move_target(target.account, folder),
                Err(error) => {
                    tracing::warn!(%error, "moving failed");
                    window.push_notification(
                        Notification::error(format!("The mail couldn't be moved: {error}"))
                            .title(PRODUCT_NAME),
                        cx,
                    );
                }
            }
        }
        self.close_quick_move(window, cx);
    }

    fn close_quick_move(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.quick_move.take().is_none() {
            return;
        }
        let focus = self
            .quick_move_return_focus
            .take()
            .unwrap_or_else(|| self.thread_list.focus_handle(cx));
        window.focus(&focus, cx);
        cx.notify();
    }

    fn notify_unavailable(what: &str, window: &mut Window, cx: &mut App) {
        window.push_notification(
            Notification::info(format!("{what} arrives with the composer.")).title(PRODUCT_NAME),
            cx,
        );
    }

    fn on_column_drag(
        &mut self,
        event: &DragMoveEvent<ColumnDrag>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let x = f32::from(event.event.position.x);
        let width = f32::from(window.viewport_size().width);
        let resolved = layout::resolve(width, &self.prefs, &self.config);
        match event.drag(cx) {
            ColumnDrag::FolderPane => {
                self.prefs.folder_pane =
                    (x - self.config.spaces_bar).max(self.config.min_folder_pane);
            }
            ColumnDrag::List => {
                let left = self.config.spaces_bar + resolved.folder_pane.unwrap_or(0.);
                self.prefs.list = (x - left).max(self.config.min_list);
            }
            ColumnDrag::Agent => {
                self.prefs.agent = (width - x).max(self.config.min_agent);
            }
        }
        cx.notify();
    }

    /// Advances the scroll benchmark by one frame.
    fn step_benchmark(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(bench) = self.bench.as_mut() else {
            return;
        };
        if bench.frame == 0 {
            self.stats.borrow_mut().start_recording();
        }
        bench.frame += 1;
        let config = bench.config;
        if bench.frame.is_multiple_of(120) {
            tracing::debug!(target: "mazemail::perf", frame = bench.frame, "scroll benchmark");
        }
        let y = px(-(bench.frame as f32) * config.pixels_per_frame);
        let list = self.thread_list.read(cx);
        match list.mode() {
            ListMode::Cards => list
                .scroll_handle()
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), y)),
            ListMode::Table => list
                .table()
                .read(cx)
                .vertical_scroll_handle
                .0
                .borrow()
                .base_handle
                .set_offset(point(px(0.), y)),
        }
        if bench.frame > config.frames {
            let (cpu, intervals) = self.stats.borrow_mut().stop_recording();
            let rss_mb = resident_memory().map(|bytes| bytes as f64 / (1024. * 1024.));
            let row_height = if config.table {
                TABLE_ROW_HEIGHT
            } else {
                CARD_HEIGHT
            };
            tracing::info!(
                target: "mazemail::perf",
                frames = config.frames,
                rows_scrolled = (config.frames as f32 * config.pixels_per_frame / row_height) as u64,
                cpu_p50_ms = cpu.map(|p| p.p50),
                cpu_p95_ms = cpu.map(|p| p.p95),
                cpu_p99_ms = cpu.map(|p| p.p99),
                cpu_max_ms = cpu.map(|p| p.max),
                interval_p50_ms = intervals.map(|p| p.p50),
                interval_p95_ms = intervals.map(|p| p.p95),
                interval_p99_ms = intervals.map(|p| p.p99),
                rss_mb,
                "scroll benchmark done"
            );
            self.bench = None;
            if config.exit {
                cx.quit();
            }
            return;
        }
        window.request_animation_frame();
    }

    /// Starts the benchmark: opens the 1M folder and scrolls from the top.
    fn start_benchmark(
        &mut self,
        config: ScrollBenchmark,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let key = crate::fake::FakeMailSource::large_folder();
        let title = self
            .folder_pane
            .read(cx)
            .title_of(&key)
            .unwrap_or_else(|| "Archives".into());
        self.folder_pane
            .update(cx, |pane, cx| pane.select(&key, cx));
        self.thread_list.update(cx, |list, cx| {
            list.open(ThreadScope::Folder(key), title, None, cx);
            if config.table != (list.mode() == ListMode::Table) {
                list.toggle_mode(window, cx);
            }
        });
        tracing::info!(target: "mazemail::perf", ?config, "starting scroll benchmark");
        self.bench = Some(BenchRun { config, frame: 0 });
        window.request_animation_frame();
    }

    fn render_title_bar(&self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let list = self.thread_list.read(cx);
        TitleBar::new()
            .bg(theme.title_bar)
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .w_full()
                    .pr_2()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .px_2()
                            .py_0p5()
                            .rounded_md()
                            .bg(theme.background)
                            .border_1()
                            .border_color(theme.border)
                            .text_xs()
                            .child(
                                Icon::new(IconName::Mail)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(list.title().clone()),
                    )
                    .child(div().flex_1())
                    .children(self.title_bar_items.iter().cloned())
                    .child(
                        div().w(px(280.)).child(
                            Input::new(&self.search).small().cleanable(true).prefix(
                                Icon::new(IconName::Search)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            ),
                        ),
                    ),
            )
    }

    fn column(
        id: &'static str,
        width: f32,
        drag: ColumnDrag,
        edge: HandleEdge,
        content: impl IntoElement,
    ) -> impl IntoElement {
        div()
            .relative()
            .h_full()
            .w(px(width))
            .flex_none()
            .child(content)
            .child(
                resize_handle::<ColumnDrag, ColumnDrag>(id, Axis::Horizontal)
                    .inside(edge)
                    .on_drag(drag, |drag, _, _, cx| cx.new(|_| *drag)),
            )
    }
}

impl Focusable for MailWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for MailWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        self.stats.borrow_mut().render_started();
        self.step_benchmark(window, cx);

        let width = f32::from(window.viewport_size().width);
        let resolved = layout::resolve(width, &self.prefs, &self.config);
        let theme = cx.theme().colors;
        let list = self.thread_list.read(cx);
        let list_len = list.key_count();
        let noun = if list.threaded() {
            "threads"
        } else {
            "messages"
        };
        let summary: SharedString = format!("{} {noun}", format_thousands(list_len as u64)).into();
        let context = self
            .message_view
            .read(cx)
            .document()
            .map(|doc| doc.subject.expose().clone());

        let list = self.thread_list.clone();
        let message = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .child(self.message_view.clone());
        let center = match resolved.message {
            MessagePlacement::Beside => h_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(Self::column(
                    "resize-list",
                    resolved.list.unwrap_or(self.prefs.list),
                    ColumnDrag::List,
                    HandleEdge::Trailing,
                    div()
                        .size_full()
                        .border_r_1()
                        .border_color(theme.border)
                        .child(list),
                ))
                .child(message)
                .into_any_element(),
            MessagePlacement::Below => v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(
                    div()
                        .h(gpui::relative(0.45))
                        .w_full()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(list),
                )
                .child(message)
                .into_any_element(),
            MessagePlacement::Hidden => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(list)
                .into_any_element(),
        };
        let agent = match resolved.agent {
            AgentPlacement::Expanded(width) => Self::column(
                "resize-agent",
                width,
                ColumnDrag::Agent,
                HandleEdge::Leading,
                AgentColumn::expanded(width, context),
            )
            .into_any_element(),
            AgentPlacement::Strip => AgentColumn::strip(self.config.agent_strip).into_any_element(),
        };

        let stats = self.stats.clone();
        let after_paint = self.after_paint.clone();
        let has_rows = list_len > 0;
        v_flex()
            .id("mail-window")
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_drag_move(cx.listener(Self::on_column_drag))
            .on_action(cx.listener(|this, _: &ToggleFolderPane, _, cx| {
                this.prefs.folder_pane_visible = !this.prefs.folder_pane_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleAgentColumn, window, cx| {
                this.prefs.agent_expanded = Some(!this.effective_agent_expanded(window));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleMessagePane, _, cx| {
                this.prefs.message_visible = !this.prefs.message_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Toggle, window, cx| this.toggle_palette(window, cx)))
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                let focus = this.search.focus_handle(cx);
                window.focus(&focus, cx);
            }))
            .on_action(cx.listener(|this, _: &QuickFilter, window, cx| {
                let input = this.thread_list.read(cx).quick_filter().clone();
                let focus = input.focus_handle(cx);
                window.focus(&focus, cx);
            }))
            .on_action(cx.listener(|this, _: &NextMessage, _, cx| {
                this.thread_list.update(cx, |list, cx| list.next(cx));
            }))
            .on_action(cx.listener(|this, _: &PreviousMessage, _, cx| {
                this.thread_list.update(cx, |list, cx| list.previous(cx));
            }))
            .on_action(cx.listener(|this, _: &NextUnread, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.jump_unread(Direction::Forward, cx));
            }))
            .on_action(cx.listener(|this, _: &PreviousUnread, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.jump_unread(Direction::Backward, cx));
            }))
            .on_action(cx.listener(|this, _: &FirstMessage, _, cx| {
                this.thread_list.update(cx, |list, cx| list.edge(false, cx));
            }))
            .on_action(cx.listener(|this, _: &LastMessage, _, cx| {
                this.thread_list.update(cx, |list, cx| list.edge(true, cx));
            }))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| {
                this.thread_list.update(cx, |list, cx| list.page(true, cx));
            }))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| {
                this.thread_list.update(cx, |list, cx| list.page(false, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleRead, _, cx| {
                this.thread_list.update(cx, |list, cx| list.toggle_read(cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleStar, _, cx| {
                this.thread_list.update(cx, |list, cx| list.toggle_star(cx));
            }))
            .on_action(cx.listener(|this, _: &Archive, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.archive_selected(cx));
            }))
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                this.thread_list.update(cx, |list, cx| list.undo(cx));
            }))
            .on_action(cx.listener(|this, _: &MoveToFolder, window, cx| {
                this.open_quick_move(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTableView, window, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.toggle_mode(window, cx));
            }))
            .on_action(cx.listener(|this, _: &ToggleThreads, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.toggle_threads(cx));
            }))
            .on_action(cx.listener(|this, _: &ExpandThread, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.expand_selected(cx));
            }))
            .on_action(cx.listener(|this, _: &CollapseThread, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.collapse_selected(cx));
            }))
            .on_action(cx.listener(|this, _: &ExpandAllThreads, _, cx| {
                this.thread_list.update(cx, |list, cx| list.expand_all(cx));
            }))
            .on_action(cx.listener(|this, _: &CollapseAllThreads, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.collapse_all(cx));
            }))
            .on_action(cx.listener(|this, _: &SelectThread, _, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.select_thread(cx));
            }))
            .on_action(cx.listener(|this, _: &ExtendSelectionDown, _, cx| {
                this.thread_list.update(cx, |list, cx| list.extend(1, cx));
            }))
            .on_action(cx.listener(|this, _: &ExtendSelectionUp, _, cx| {
                this.thread_list.update(cx, |list, cx| list.extend(-1, cx));
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
                this.thread_list.update(cx, |list, cx| list.select_all(cx));
            }))
            .on_action(|_: &Reply, window, cx| Self::notify_unavailable("Reply", window, cx))
            .on_action(|_: &ReplyAll, window, cx| Self::notify_unavailable("Reply all", window, cx))
            .on_action(|_: &Forward, window, cx| Self::notify_unavailable("Forward", window, cx))
            .on_action(|_: &Minimize, window, _| window.minimize_window())
            .on_action(|_: &Zoom, window, _| window.zoom_window())
            .on_action(|_: &About, window, cx| {
                let detail = format!(
                    "Version {}\nA local-first email client.",
                    env!("CARGO_PKG_VERSION")
                );
                // The answer doesn't matter; dropping the receiver is fine.
                drop(window.prompt(PromptLevel::Info, PRODUCT_NAME, Some(&detail), &["OK"], cx));
            })
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(SpacesBar::new(self.config.spaces_bar))
                    .when_some(resolved.folder_pane, |this, width| {
                        this.child(Self::column(
                            "resize-folders",
                            width,
                            ColumnDrag::FolderPane,
                            HandleEdge::Trailing,
                            div()
                                .size_full()
                                .border_r_1()
                                .border_color(theme.border)
                                .child(self.folder_pane.clone()),
                        ))
                    })
                    .child(center)
                    .child(agent),
            )
            .child(MailStatusBar::new(self.source.status(), summary))
            .children(self.palette.clone())
            .children(self.quick_move.clone())
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, _, cx| {
                        stats.borrow_mut().painted(has_rows);
                        for callback in after_paint.borrow_mut().drain(..) {
                            cx.defer(callback);
                        }
                    },
                )
                .absolute()
                .size_0(),
            )
            .map(|this| {
                if let Some(config) = self.pending_benchmark.take() {
                    // Start after this first frame is on screen.
                    let entity = cx.entity();
                    window.on_next_frame(move |window, cx| {
                        entity.update(cx, |this, cx| this.start_benchmark(config, window, cx));
                    });
                }
                this
            })
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use time::macros::datetime;

    use super::*;
    use crate::fake::FakeMailSource;

    pub(crate) fn open(cx: &mut TestAppContext) -> (Entity<MailWindow>, &mut VisualTestContext) {
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let source: Arc<dyn MailSource> =
            Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
        cx.add_window_view(|window, cx| {
            MailWindow::new(source, &MailWindowOptions::default(), window, cx)
        })
    }

    #[gpui::test]
    fn keymap_drives_selection_and_layout(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let selected = |cx: &mut VisualTestContext| {
            view.read_with(cx, |w, cx| w.thread_list().read(cx).selected_index())
        };
        assert_eq!(selected(cx), Some(0));
        cx.simulate_keystrokes("f f");
        assert_eq!(selected(cx), Some(2));
        cx.simulate_keystrokes("b");
        assert_eq!(selected(cx), Some(1));
        cx.simulate_keystrokes("n");
        let after_n = selected(cx).unwrap();
        assert!(after_n > 1);
        // The message view follows the selection.
        let shown = view.read_with(cx, |w, cx| {
            w.message_view().read(cx).document().map(|d| d.thread)
        });
        let expected = view.read_with(cx, |w, cx| w.thread_list().read(cx).selected_thread());
        assert_eq!(shown, expected);

        // Workspace bindings toggle columns.
        assert!(view.read_with(cx, |w, _| w.prefs().folder_pane_visible));
        cx.simulate_keystrokes("cmd-b");
        assert!(!view.read_with(cx, |w, _| w.prefs().folder_pane_visible));
        cx.simulate_keystrokes("f8");
        assert!(!view.read_with(cx, |w, _| w.prefs().message_visible));
        cx.simulate_keystrokes("cmd-?");
        assert!(view.read_with(cx, |w, _| w.prefs().agent_expanded.is_some()));
    }

    #[gpui::test]
    fn thunderbird_keys_drive_the_thread_tree(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let list = view.read_with(cx, |w, _| w.thread_list().clone());
        // A thread with replies (the fake's data is the same for the same "now").
        let fake = FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC));
        let row = list.update(cx, |list, cx| {
            let crate::thread_list::ListKeys::Threads(keys) = list.model().borrow().keys().clone()
            else {
                panic!("threaded");
            };
            let row = fake
                .thread_rows(&keys[..200])
                .iter()
                .position(|r| r.message_count >= 3)
                .unwrap();
            list.select(row, cx);
            row
        });
        let state = |cx: &mut VisualTestContext| {
            list.read_with(cx, |l, _| (l.selected_index(), l.len(), l.is_expanded(row)))
        };
        let (_, len, _) = state(cx);
        cx.simulate_keystrokes("right");
        let (selected, expanded_len, expanded) = state(cx);
        assert!(expanded && expanded_len > len && selected == Some(row));
        cx.simulate_keystrokes("right");
        assert_eq!(state(cx).0, Some(row + 1), "into the thread");
        // The message view shows the selected message, not the thread's newest.
        let (shown, item) = view.read_with(cx, |w, cx| {
            (
                w.message_view().read(cx).document().map(|d| d.message),
                w.thread_list().read(cx).selected_item(),
            )
        });
        assert_eq!(item, shown.map(ItemId::Message));
        cx.simulate_keystrokes("left");
        assert_eq!(state(cx).0, Some(row));
        cx.simulate_keystrokes("left");
        assert_eq!(state(cx), (Some(row), len, false));

        cx.simulate_keystrokes("*");
        assert!(list.read_with(cx, |l, _| l.len()) > len, "expand all");
        cx.simulate_keystrokes("\\");
        assert_eq!(list.read_with(cx, |l, _| l.len()), len, "collapse all");

        cx.simulate_keystrokes("right right cmd-shift-a");
        assert_eq!(state(cx).0, Some(row));
        assert!(list.read_with(cx, |l, _| l.selected_whole_thread().is_some()));

        // The table view: its own left/right don't take over.
        cx.simulate_keystrokes("cmd-alt-t");
        assert_eq!(list.read_with(cx, |l, _| l.mode()), ListMode::Table);
        cx.simulate_keystrokes("left");
        assert!(!list.read_with(cx, |l, _| l.is_expanded(row)));
        cx.simulate_keystrokes("right");
        assert!(list.read_with(cx, |l, _| l.is_expanded(row)));

        // Threads off.
        cx.simulate_keystrokes("cmd-alt-g");
        assert!(!list.read_with(cx, |l, _| l.threaded()));
    }

    #[gpui::test]
    fn a_remembered_table_view_gets_room_at_start(cx: &mut TestAppContext) {
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let ui_state = UiStateStore::in_memory(ListDefaults::default());
        let mut prefs = ui_state.prefs();
        prefs.mode = ListMode::Table;
        ui_state.set_prefs(&prefs);
        let source: Arc<dyn MailSource> =
            Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
        let options = MailWindowOptions {
            ui_state,
            ..MailWindowOptions::default()
        };
        let (view, cx) =
            cx.add_window_view(|window, cx| MailWindow::new(source, &options, window, cx));
        view.read_with(cx, |w, cx| {
            assert_eq!(w.thread_list().read(cx).mode(), ListMode::Table);
            assert!(w.prefs().list >= TABLE_LIST_WIDTH);
        });
    }

    /// App-level handlers for actions run from the palette (or a button) are called
    /// while this window handles the event, when GPUI can't update the window. The
    /// app defers such handlers (`mazemail::app`, Add Account): this pins down why.
    #[gpui::test]
    fn palette_actions_must_defer_window_updates(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        let (_view, cx) = open(cx);
        cx.run_until_parked();
        let handle = cx.update(|window, _| window.window_handle());
        let results: Rc<RefCell<Vec<(&str, bool)>>> = Rc::default();
        cx.update(|_, cx| {
            let results = results.clone();
            cx.on_action(move |_: &crate::actions::AddAccount, cx| {
                let direct = handle.update(cx, |_, _, _| ()).is_ok();
                results.borrow_mut().push(("direct", direct));
                let results = results.clone();
                cx.defer(move |cx| {
                    let deferred = handle.update(cx, |_, _, _| ()).is_ok();
                    results.borrow_mut().push(("deferred", deferred));
                });
            });
        });
        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("add account");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            *results.borrow(),
            vec![("direct", false), ("deferred", true)],
            "the palette ran Add Account inside the window's event handling"
        );
    }

    /// The palette shows a binding with a context (`a` for Archive in the list), which
    /// only matches where the focus was before the palette took it.
    #[gpui::test]
    fn palette_hints_come_from_the_previous_focus(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let list = view.read_with(cx, |w, cx| w.thread_list().focus_handle(cx));
        cx.update(|window, cx| window.focus(&list, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-p");
        cx.update(|window, cx| {
            let palette = view.read(cx).palette.clone().unwrap();
            let palette_focus = palette.focus_handle(cx);
            let archive = crate::actions::Archive;
            let hint = crate::command_palette::binding_hint(&archive, Some(&list), window);
            assert_eq!(hint.map(|k| k.key), Some("a".to_owned()));
            // From the palette's own focus the list's bindings don't apply.
            assert_eq!(
                crate::command_palette::binding_hint(&archive, Some(&palette_focus), window),
                None
            );
        });
    }

    #[gpui::test]
    fn cmd_z_undoes_an_archive(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let list = view.read_with(cx, |w, cx| w.thread_list().focus_handle(cx));
        cx.update(|window, cx| window.focus(&list, cx));
        let rows = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |w, cx| w.thread_list().read(cx).len())
        };
        let before = rows(cx);
        let first = view.read_with(cx, |w, cx| w.thread_list().read(cx).selected_thread());
        cx.simulate_keystrokes("a");
        cx.run_until_parked();
        assert_eq!(rows(cx), before - 1);
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();
        assert_eq!(rows(cx), before);
        // Nothing left to undo: a second `cmd-z` changes nothing.
        cx.simulate_keystrokes("cmd-z");
        cx.run_until_parked();
        assert_eq!(rows(cx), before);
        cx.simulate_keystrokes("home");
        let top = view.read_with(cx, |w, cx| w.thread_list().read(cx).selected_thread());
        assert_eq!(top, first, "the archived thread is back at the top");
    }

    #[gpui::test]
    fn palette_opens_runs_actions_and_closes(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-p");
        assert!(view.read_with(cx, |w, _| w.is_palette_open()));
        // Single-key shortcuts type into the palette instead of moving the selection.
        cx.simulate_keystrokes("f");
        assert_eq!(
            view.read_with(cx, |w, cx| w.thread_list().read(cx).selected_index()),
            Some(0)
        );
        cx.simulate_keystrokes("escape escape");
        assert!(!view.read_with(cx, |w, _| w.is_palette_open()));
        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_keystrokes("cmd-shift-p");
        assert!(!view.read_with(cx, |w, _| w.is_palette_open()));
    }

    /// The Trash folder of the account of `thread`, and the account's Inbox.
    fn trash_and_inbox(
        source: &FakeMailSource,
        thread: mazemail_types::ThreadId,
    ) -> (mazemail_types::FolderId, mazemail_types::FolderId) {
        let account = source.thread_account(thread).unwrap();
        let tree = source.folder_tree();
        let folders = &tree
            .accounts
            .iter()
            .find(|a| a.id == account)
            .unwrap()
            .folders;
        let with_role = |role| match folders.iter().find(|f| f.role == Some(role)).unwrap().key {
            FolderKey::Folder(id) => id,
            FolderKey::Unified(_) => unreachable!(),
        };
        (with_role(FolderRole::Trash), with_role(FolderRole::Inbox))
    }

    #[gpui::test]
    fn shift_m_moves_the_selection_to_a_picked_folder(cx: &mut TestAppContext) {
        use crate::fake::MoveItem;

        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let fake = Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
        let source: Arc<dyn MailSource> = fake.clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            MailWindow::new(source, &MailWindowOptions::default(), window, cx)
        });
        cx.run_until_parked();
        let selected = |cx: &mut VisualTestContext| {
            view.read_with(cx, |w, cx| w.thread_list().read(cx).selected_thread())
        };
        let thread = selected(cx).unwrap();
        let (trash, inbox) = trash_and_inbox(&fake, thread);
        let offered = |cx: &mut VisualTestContext| {
            view.read_with(cx, |w, cx| {
                w.quick_move()
                    .map(|picker| picker.read(cx).matches().map(|c| c.id).collect::<Vec<_>>())
            })
        };

        cx.simulate_keystrokes("shift-m");
        let all = offered(cx).expect("the picker is open");
        assert!(!all.contains(&inbox), "the Inbox shown is not a target");
        assert!(all.contains(&trash));
        // Single-key shortcuts type into the filter.
        cx.simulate_input("trash");
        assert_eq!(offered(cx).unwrap(), [trash]);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(offered(cx).is_none(), "the picker closed");
        assert_eq!(fake.moved_to(MoveItem::Thread(thread)), Some(trash));
        let next = selected(cx).unwrap();
        assert_ne!(next, thread, "the next thread took its place");

        // Focus is back on the list, and Trash is now a recent target: first.
        cx.simulate_keystrokes("shift-m");
        let (next_trash, _) = trash_and_inbox(&fake, next);
        if next_trash == trash {
            assert_eq!(offered(cx).unwrap()[0], trash);
        }
        // Escape clears the filter, then closes without moving.
        cx.simulate_input("x");
        cx.simulate_keystrokes("escape escape");
        assert!(offered(cx).is_none());
        assert_eq!(fake.moved_to(MoveItem::Thread(next)), None);
        assert_eq!(selected(cx), Some(next));

        // From the palette: the picker takes its place and gets the keys.
        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("move to folder");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(!view.read_with(cx, |w, _| w.is_palette_open()));
        assert!(offered(cx).is_some());
        cx.simulate_input("trash");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(fake.moved_to(MoveItem::Thread(next)), Some(next_trash));
        assert!(offered(cx).is_none());
    }

    #[gpui::test]
    fn shift_arrows_select_rows_that_quick_move_moves_together(cx: &mut TestAppContext) {
        use crate::fake::MoveItem;

        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let fake = Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
        let source: Arc<dyn MailSource> = fake.clone();
        let (view, cx) = cx.add_window_view(|window, cx| {
            MailWindow::new(source, &MailWindowOptions::default(), window, cx)
        });
        cx.run_until_parked();
        // Gmail (personal) / Inbox: one account, so Quick Move offers its folders.
        let inbox = FolderKey::Folder(mazemail_types::FolderId(1));
        view.update_in(cx, |w, window, cx| {
            let pane = w.folder_pane.clone();
            let selected = FolderPaneEvent::Selected {
                key: inbox,
                title: "Inbox".into(),
            };
            w.on_folder_pane_event(&pane, &selected, window, cx);
        });
        let items = |cx: &mut VisualTestContext| {
            view.update(cx, |w, cx| {
                w.thread_list().update(cx, |l, _| l.command_items())
            })
        };
        let first = items(cx);
        cx.simulate_keystrokes("shift-down shift-down shift-up");
        let selected = items(cx);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0], first[0]);
        assert_eq!(
            view.read_with(cx, |w, cx| w.message_view().read(cx).selection()),
            Some(2),
            "the message view says how many are selected"
        );
        // A plain arrow collapses; cmd-a selects everything.
        cx.simulate_keystrokes("down");
        assert_eq!(items(cx).len(), 1);
        cx.simulate_keystrokes("cmd-a");
        let all = view.read_with(cx, |w, cx| w.thread_list().read(cx).len());
        assert_eq!(
            view.read_with(cx, |w, cx| w.thread_list().read(cx).selected_count()),
            all
        );
        cx.simulate_keystrokes("up shift-up");
        let selected = items(cx);
        assert_eq!(selected.len(), 2);

        cx.simulate_keystrokes("shift-m");
        cx.simulate_input("trash");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let trash = mazemail_types::FolderId(8);
        for item in selected {
            let ItemId::Thread(thread) = item else {
                panic!("thread rows");
            };
            assert_eq!(fake.moved_to(MoveItem::Thread(thread)), Some(trash));
        }
        assert_eq!(items(cx).len(), 1, "one row selected after the move");
        assert_eq!(
            view.read_with(cx, |w, cx| w.message_view().read(cx).selection()),
            None
        );

        // The table extends the same way; its own up/down collapse.
        cx.simulate_keystrokes("cmd-alt-t shift-down shift-down");
        assert_eq!(items(cx).len(), 3);
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(items(cx).len(), 1);
    }

    #[gpui::test]
    fn selecting_a_folder_opens_it(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        view.update_in(cx, |w, window, cx| {
            let key = FakeMailSource::large_folder();
            w.on_folder_pane_event(
                &w.folder_pane.clone(),
                &FolderPaneEvent::Selected {
                    key: key.clone(),
                    title: "Archives".into(),
                },
                window,
                cx,
            );
            assert_eq!(w.thread_list().read(cx).len(), 1_000_000);
            assert_eq!(w.thread_list().read(cx).scope(), &ThreadScope::Folder(key));
        });
    }
    #[gpui::test]
    fn runs_callbacks_after_the_next_paint(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let ran = Rc::new(std::cell::Cell::new(0));
        let counter = ran.clone();
        view.update(cx, |w, cx| {
            w.after_next_paint(move |_| counter.set(counter.get() + 1), cx);
            // Not before the frame.
            assert_eq!(w.after_paint.borrow().len(), 1);
        });
        cx.run_until_parked();
        assert_eq!(ran.get(), 1);
        // Once only.
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(ran.get(), 1);
    }

    #[gpui::test]
    fn shows_title_bar_items(cx: &mut TestAppContext) {
        struct Item;
        impl Render for Item {
            fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
                div()
                    .debug_selector(|| "title-bar-item".into())
                    .child("Restart to update")
            }
        }
        let (view, cx) = open(cx);
        let item = cx.new(|_| Item);
        view.update(cx, |w, cx| w.add_title_bar_item(item.into(), cx));
        cx.run_until_parked();
        assert!(cx.debug_bounds("title-bar-item").is_some());
    }
}

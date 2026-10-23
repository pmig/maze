//! The Maze main window: title bar, spaces bar, folder pane, message list, message
//! view, agent column and status bar (CONCEPT §8.2), plus the command palette.

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
use crate::source::{DataChanges, Direction, FolderKey, MailSource, ThreadScope};
use crate::thread_list::{
    CARD_HEIGHT, ListMode, TABLE_LIST_WIDTH, TABLE_ROW_HEIGHT, ThreadList, ThreadListEvent,
};

/// How the main window starts.
#[derive(Debug, Clone, Copy)]
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
}

impl Default for MailWindowOptions {
    fn default() -> Self {
        Self {
            local_offset: UtcOffset::UTC,
            process_start: Instant::now(),
            benchmark: None,
            window_size: None,
        }
    }
}

/// Default window size; macOS shrinks it to fit smaller screens.
const DEFAULT_SIZE: (f32, f32) = (1680., 1000.);
/// The window can't get smaller than this.
const MIN_SIZE: (f32, f32) = (720., 480.);

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
    gpui_kit::open_window(window_options, cx, |window, cx| {
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
            cards_list_width: None,
            stats: Rc::new(RefCell::new(FrameStats::new(options.process_start))),
            pending_benchmark: options.benchmark,
            bench: None,
            title_bar_items: Vec::new(),
            after_paint: Rc::default(),
            _subscriptions: subscriptions,
        };
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
        let before = self.thread_list.read(cx).selected_thread();
        if changes.everything || changes.threads || !changes.threads_updated.is_empty() {
            self.thread_list.update(cx, |list, cx| list.refresh(cx));
        }
        // A changed selection was shown through the list's event already.
        let selected = self.thread_list.read(cx).selected_thread();
        let showing_doc = self.message_view.read(cx).document();
        let showing = showing_doc.map(|doc| doc.thread);
        let message_stale =
            showing_doc.is_some_and(|doc| changes.messages_updated.contains(&doc.message));
        let stale =
            message_stale || selected.is_some_and(|id| changes.threads_updated.contains(&id));
        if selected == before && (changes.everything || stale || showing != selected) {
            self.show_selected(cx);
        }
        cx.notify();
    }

    fn show_selected(&mut self, cx: &mut Context<'_, Self>) {
        let thread = self.thread_list.read(cx).selected_thread();
        let document = thread.map(|id| self.source.message(id));
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
            ThreadListEvent::ModeChanged(ListMode::Table) => {
                // The table needs room for its columns; remember the card width.
                self.cards_list_width = Some(self.prefs.list);
                self.prefs.list = self.prefs.list.max(TABLE_LIST_WIDTH);
            }
            ThreadListEvent::ModeChanged(ListMode::Cards) => {
                if let Some(width) = self.cards_list_width.take() {
                    self.prefs.list = width;
                }
            }
        }
        cx.notify();
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
        let palette = cx.new(|cx| CommandPalette::new(window, cx));
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
        let list_len = self.thread_list.read(cx).len();
        let summary: SharedString = format!("{} threads", format_thousands(list_len as u64)).into();
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
            .on_action(cx.listener(|this, _: &ToggleTableView, window, cx| {
                this.thread_list
                    .update(cx, |list, cx| list.toggle_mode(window, cx));
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

    fn open(cx: &mut TestAppContext) -> (Entity<MailWindow>, &mut VisualTestContext) {
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

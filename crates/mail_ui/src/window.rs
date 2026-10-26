//! The Maze main window: title bar, spaces bar, folder pane, the center with its tabs
//! and splits, the agent column and the status bar (CONCEPT §8.2, ADR-018), plus the
//! command palette and the Quick Move picker.
//!
//! # The center
//!
//! The center is a [`PaneGroup`]: panes in splits, each with a tab bar. Its tabs are
//! [`MailView`]s (list and message of a folder), [`MessageTab`]s (one message, full
//! width) and drafts (the `composer` crate's view; the app opens them, see
//! [`ComposeRequest`], and registers how they are restored). The agent column stays to
//! the right of all of it.
//!
//! - Picking a folder opens it in the active mail view (or the one used last, or a new
//!   one); "Open in New Tab" in the folder's menu opens another.
//! - Mail actions (`a`, `m`, `s`, `shift-m`, `cmd-z`, Reply, …) act on the active tab,
//!   wherever they come from (keys, menus, the palette).
//! - The layout is saved to `ui_state.json` a moment after it changes and restored when
//!   the window opens.
//! - "Maze: Open Settings" (`cmd-,`) and "Maze: Open Default Settings" open
//!   [`SettingsEditor`] tabs ([`MailWindowOptions::settings_file`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyView, AnyWindowHandle, App, AppContext as _, Axis, Context, DragMoveEvent, Empty, Entity,
    EntityId, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    PromptLevel, Render, SharedString, Styled, Subscription, Task, Window, WindowBounds,
    WindowOptions, canvas, div, point, prelude::FluentBuilder as _, px, size,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, TitleBar, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    resizable::{HandleEdge, resize_handle},
    v_flex,
};
use mazemail_types::{FolderRole, MessageId, PRODUCT_NAME, ThreadId};
use time::UtcOffset;
use ui::assets::IconName;
use ui::names::NameRng;
use ui::panes::{
    EmptyState, ItemHandle, ItemKind, Pane, PaneGroup, PaneGroupEvent, Restore, SerializedItem,
};

use crate::actions::*;
use crate::chrome::{AgentColumn, MailStatusBar, SpacesBar};
use crate::command_palette::{CommandPalette, Dismissed};
use crate::folder_pane::{FolderPane, FolderPaneEvent};
use crate::format::format_thousands;
use crate::layout::{self, AgentPlacement, LayoutConfig, LayoutPrefs};
use crate::mail_view::{MAIL_KIND, MailView, MailViewContext, MailViewEvent, SerializedFolder};
use crate::message_tab::{MESSAGE_KIND, MessageTab, SerializedMessage};
use crate::message_view::MessageView;
use crate::perf::{FrameStats, ScrollBenchmark, resident_memory};
use crate::quick_move::{MoveTarget, QuickMove, QuickMoveEvent, source_folder};
use crate::settings_editor::{SETTINGS_KIND, SerializedSettings, SettingsEditor, SettingsFile};
use crate::source::{DataChanges, Direction, FolderKey, MailSource, SortBy, ThreadScope};
use crate::thread_list::{CARD_HEIGHT, ItemId, ListMode, TABLE_ROW_HEIGHT, ThreadList};
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
    /// The message list view and the layout (`ui_state.json`); in memory by default.
    pub ui_state: UiStateStore,
    /// The user's `settings.json`, edited in a tab ("Maze: Open Settings"). `None`
    /// (tests, the demo): only the defaults can be shown.
    pub settings_file: Option<SettingsFile>,
}

impl Default for MailWindowOptions {
    fn default() -> Self {
        Self {
            local_offset: UtcOffset::UTC,
            process_start: Instant::now(),
            benchmark: None,
            window_size: None,
            ui_state: UiStateStore::in_memory(ListDefaults::default()),
            settings_file: None,
        }
    }
}

/// Default window size; macOS shrinks it to fit smaller screens.
const DEFAULT_SIZE: (f32, f32) = (1680., 1000.);
/// The window can't get smaller than this.
const MIN_SIZE: (f32, f32) = (720., 480.);
/// Selected rows whose account Quick Move checks before it opens.
const ACCOUNT_CHECK_ROWS: usize = 1_000;
/// The layout is saved this long after its last change.
const LAYOUT_SAVE_DELAY: Duration = Duration::from_millis(400);

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

/// What the window asks the app to open a draft for: a new message (`cmd-n`, the
/// title bar's "+"), or Reply, Reply All, Forward on the message of the active tab.
/// The app opens the composer in a tab ([`MailWindow::add_item`]) named with
/// [`MailWindow::new_draft_name`], so this crate doesn't depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposeRequest {
    /// A blank message.
    New,
    /// Reply to the sender, or to everyone.
    Reply {
        /// The message replied to.
        message: MessageId,
        /// Reply to all recipients.
        all: bool,
    },
    /// Forward a message.
    Forward {
        /// The message forwarded.
        message: MessageId,
    },
}

impl EventEmitter<ComposeRequest> for MailWindow {}

/// The main window couldn't be opened.
#[derive(Debug, thiserror::Error)]
#[error("could not open the main window: {0}")]
pub struct OpenWindowError(pub String);

/// A column being resized; also the drag payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnDrag {
    FolderPane,
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

/// What Quick Move moves: rows of a list, or the message of a tab.
#[derive(Debug, Clone)]
enum MoveOwner {
    List(Entity<ThreadList>),
    Message(Entity<MessageTab>),
}

/// The main window's root view.
pub struct MailWindow {
    source: Arc<dyn MailSource>,
    focus: FocusHandle,
    folder_pane: Entity<FolderPane>,
    panes: Entity<PaneGroup>,
    views: MailViewContext,
    search: Entity<InputState>,
    prefs: Rc<RefCell<LayoutPrefs>>,
    config: LayoutConfig,
    palette: Option<Entity<CommandPalette>>,
    palette_return_focus: Option<FocusHandle>,
    quick_move: Option<Entity<QuickMove>>,
    quick_move_owner: Option<MoveOwner>,
    quick_move_return_focus: Option<FocusHandle>,
    ui_state: UiStateStore,
    stats: Rc<RefCell<FrameStats>>,
    pending_benchmark: Option<ScrollBenchmark>,
    bench: Option<BenchRun>,
    title_bar_items: Vec<AnyView>,
    agent_panel: Option<AnyView>,
    after_paint: Rc<RefCell<Vec<AfterPaint>>>,
    names: NameRng,
    settings_file: Option<SettingsFile>,
    save_task: Option<Task<()>>,
    item_subscriptions: HashMap<EntityId, Subscription>,
    _subscriptions: Vec<Subscription>,
}

/// A callback for after the next frame was painted.
type AfterPaint = Box<dyn FnOnce(&mut App)>;

impl std::fmt::Debug for MailWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailWindow").finish_non_exhaustive()
    }
}

/// The title of a folder, from the folder pane, or the role's name for a unified
/// folder the pane doesn't show (yet).
fn folder_title(pane: &FolderPane, key: &FolderKey) -> Option<SharedString> {
    pane.title_of(key).or_else(|| match key {
        FolderKey::Unified(role) => Some(role_title(*role)),
        FolderKey::Folder(_) => None,
    })
}

fn role_title(role: FolderRole) -> SharedString {
    let name = role.as_str();
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()).into(),
        None => SharedString::default(),
    }
}

impl MailWindow {
    /// Builds the window content and restores the saved tabs (or opens the unified
    /// Inbox).
    pub fn new(
        source: Arc<dyn MailSource>,
        options: &MailWindowOptions,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let initial = FolderKey::Unified(FolderRole::Inbox);
        let folder_pane = cx.new(|cx| FolderPane::new(source.clone(), &initial, window, cx));
        let prefs = Rc::new(RefCell::new(LayoutPrefs::default()));
        let config = LayoutConfig::default();
        let views = MailViewContext {
            source: source.clone(),
            local_offset: options.local_offset,
            ui_state: options.ui_state.clone(),
            prefs: prefs.clone(),
            config,
        };
        let empty_state: EmptyState = Rc::new(|_, cx| {
            let theme = cx.theme().colors;
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No open tabs")
                .child(
                    div()
                        .text_xs()
                        .child("Pick a folder, or press ⌘N to write a message."),
                )
                .into_any_element()
        });
        let panes = cx.new(|cx| PaneGroup::new(Some(empty_state), window, cx));
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search all messages…  ⌘K")
                .clean_on_escape()
        });
        let subscriptions = vec![
            cx.subscribe_in(&folder_pane, window, Self::on_folder_pane_event),
            cx.subscribe_in(&panes, window, Self::on_pane_group_event),
            cx.subscribe_in(&search, window, Self::on_search_event),
            ui::theme::observe_system_appearance(window),
        ];
        let mut this = Self {
            source,
            focus: cx.focus_handle(),
            folder_pane,
            panes,
            views,
            search,
            prefs,
            config,
            palette: None,
            palette_return_focus: None,
            quick_move: None,
            quick_move_owner: None,
            quick_move_return_focus: None,
            ui_state: options.ui_state.clone(),
            stats: Rc::new(RefCell::new(FrameStats::new(options.process_start))),
            pending_benchmark: options.benchmark,
            bench: None,
            title_bar_items: Vec::new(),
            agent_panel: None,
            after_paint: Rc::default(),
            names: NameRng::from_entropy(),
            settings_file: options.settings_file.clone(),
            save_task: None,
            item_subscriptions: HashMap::new(),
            _subscriptions: subscriptions,
        };
        this.register_kinds(window, cx);
        let restored = this.ui_state.layout().is_some_and(|layout| {
            this.panes
                .update(cx, |panes, cx| panes.load(layout, window, cx))
        });
        let has_items = !this.panes.read(cx).items(cx).is_empty();
        if !restored || !has_items {
            let title =
                folder_title(this.folder_pane.read(cx), &initial).unwrap_or_else(|| "Inbox".into());
            let view = this.new_mail_view(ThreadScope::Folder(initial), title, None, window, cx);
            this.panes.update(cx, |panes, cx| {
                panes.add_item(Box::new(view), true, window, cx)
            });
        }
        this.track_items(window, cx);
        this.sync_folder_selection(cx);
        let pane = this.panes.read(cx).active_pane().clone();
        pane.update(cx, |pane, cx| pane.focus_active_item(window, cx));
        this
    }

    /// Mail views and message tabs come back from the layout file; a keyboard split of
    /// a draft gets a mail view of the selected folder.
    fn register_kinds(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let views = self.views.clone();
        let folder_pane = self.folder_pane.downgrade();
        let titles = self.folder_pane.downgrade();
        let mail = ItemKind::new(move |item: &SerializedItem, window, cx| {
            let Ok(folder) = serde_json::from_value::<SerializedFolder>(item.data.clone()) else {
                return Restore::Gone;
            };
            let key = folder.key();
            let Some(pane) = folder_pane.upgrade() else {
                return Restore::Later;
            };
            let title = folder_title(pane.read(cx), &key);
            let title = match title {
                Some(title) => title,
                // The folders aren't read yet.
                None if views.source.placeholder().is_some() => return Restore::Later,
                None => return Restore::Gone,
            };
            let views = views.clone();
            let view = cx
                .new(|cx| MailView::new(views, ThreadScope::Folder(key), title, None, window, cx));
            Restore::Item(Box::new(view))
        })
        .icon(IconName::Inbox)
        .title(move |item, cx| {
            let folder = serde_json::from_value::<SerializedFolder>(item.data.clone()).ok()?;
            folder_title(titles.upgrade()?.read(cx), &folder.key())
        });

        let source = self.source.clone();
        let offset = self.views.local_offset;
        let message = ItemKind::new(move |item: &SerializedItem, _, cx| {
            let Ok(SerializedMessage { message }) =
                serde_json::from_value::<SerializedMessage>(item.data.clone())
            else {
                return Restore::Gone;
            };
            if !MessageTab::exists(source.as_ref(), message) {
                return Restore::Gone;
            }
            let source = source.clone();
            Restore::Item(Box::new(
                cx.new(|cx| MessageTab::new(source, offset, message, cx)),
            ))
        })
        .icon(IconName::Mail)
        .title(|_, _| Some("Message".into()));

        let settings_file = self.settings_file.clone();
        let settings = ItemKind::new(move |item: &SerializedItem, window, cx| {
            let serialized =
                serde_json::from_value::<SerializedSettings>(item.data.clone()).unwrap_or_default();
            let editor = match (serialized.defaults, &settings_file) {
                (true, _) => cx.new(|cx| SettingsEditor::defaults(window, cx)),
                (false, Some(file)) => {
                    let file = file.clone();
                    cx.new(|cx| SettingsEditor::open(file, window, cx))
                }
                (false, None) => return Restore::Gone,
            };
            Restore::Item(Box::new(editor))
        })
        .icon(IconName::Settings)
        .title(|item, _| {
            let serialized =
                serde_json::from_value::<SerializedSettings>(item.data.clone()).ok()?;
            Some(if serialized.defaults {
                "Default Settings".into()
            } else {
                "Settings".into()
            })
        });

        let views = self.views.clone();
        let fallback_pane = self.folder_pane.downgrade();
        self.panes.update(cx, |panes, cx| {
            panes.register_kind(MAIL_KIND, mail, window, cx);
            panes.register_kind(MESSAGE_KIND, message, window, cx);
            panes.register_kind(SETTINGS_KIND, settings, window, cx);
            panes.set_split_fallback(Rc::new(move |window, cx| {
                let pane = fallback_pane.upgrade()?;
                let key = pane
                    .read(cx)
                    .selected()
                    .cloned()
                    .unwrap_or(FolderKey::Unified(FolderRole::Inbox));
                let title = folder_title(pane.read(cx), &key).unwrap_or_else(|| "Inbox".into());
                let views = views.clone();
                let view = cx.new(|cx| {
                    MailView::new(views, ThreadScope::Folder(key), title, None, window, cx)
                });
                Some(Box::new(view) as Box<dyn ItemHandle>)
            }));
        });
    }

    fn new_mail_view(
        &self,
        scope: ThreadScope,
        title: SharedString,
        search: Option<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<MailView> {
        let views = self.views.clone();
        cx.new(|cx| MailView::new(views, scope, title, search, window, cx))
    }

    // ---- public API ----------------------------------------------------------------

    /// The center's panes and tabs.
    pub fn panes(&self) -> &Entity<PaneGroup> {
        &self.panes
    }

    /// The active tab, if it is a mail view.
    pub fn active_mail_view(&self, cx: &App) -> Option<Entity<MailView>> {
        self.panes
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.downcast::<MailView>())
    }

    /// The active tab, if it is a message.
    pub fn active_message_tab(&self, cx: &App) -> Option<Entity<MessageTab>> {
        self.panes
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.downcast::<MessageTab>())
    }

    /// The mail view mail actions and folder picks go to: the active tab, else the
    /// mail view used last.
    pub fn current_mail_view(&self, cx: &App) -> Option<Entity<MailView>> {
        self.active_mail_view(cx)
            .or_else(|| self.panes.read(cx).most_recent::<MailView>(cx))
    }

    /// The message list of the current mail view.
    pub fn thread_list(&self, cx: &App) -> Option<Entity<ThreadList>> {
        self.current_mail_view(cx)
            .map(|view| view.read(cx).thread_list().clone())
    }

    /// The message view of the current mail view.
    pub fn message_view(&self, cx: &App) -> Option<Entity<MessageView>> {
        self.current_mail_view(cx)
            .map(|view| view.read(cx).message_view().clone())
    }

    /// The folder pane.
    pub fn folder_pane(&self) -> &Entity<FolderPane> {
        &self.folder_pane
    }

    /// The layout preferences (column widths and toggles).
    pub fn prefs(&self) -> LayoutPrefs {
        *self.prefs.borrow()
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

    /// Shows `view` as the expanded agent column's content instead of the setup card
    /// (the `agent_ui` panel; the binary plugs it in so the window doesn't depend on
    /// it). The view brings its own header.
    pub fn set_agent_panel(&mut self, view: AnyView, cx: &mut Context<'_, Self>) {
        self.agent_panel = Some(view);
        cx.notify();
    }

    /// Opens `item` (a draft, from the app) in a tab of the active pane and focuses it.
    pub fn add_item(
        &mut self,
        item: Box<dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.panes
            .update(cx, |panes, cx| panes.add_item(item, true, window, cx));
    }

    /// Registers how tabs of `kind` come back from the layout file (the app's drafts),
    /// and restores the shown tabs of that kind.
    pub fn register_item_kind(
        &mut self,
        kind: &str,
        item_kind: ItemKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.panes.update(cx, |panes, cx| {
            panes.register_kind(kind, item_kind, window, cx)
        });
    }

    /// Shows the settings in a tab: the user's `settings.json`, or the read-only
    /// defaults. An open tab of the same kind is activated (and focused) instead.
    pub fn open_settings(
        &mut self,
        defaults: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Entity<SettingsEditor>> {
        let open = self
            .panes
            .read(cx)
            .items_of::<SettingsEditor>(cx)
            .into_iter()
            .find(|editor| editor.read(cx).is_defaults() == defaults);
        if let Some(editor) = open {
            let id = editor.entity_id();
            self.panes
                .update(cx, |panes, cx| panes.activate_item(id, true, window, cx));
            return Some(editor);
        }
        // A tab from the layout that isn't restored yet: show it, which restores it.
        let pending = self
            .panes
            .read(cx)
            .items_of::<ui::panes::PendingItem>(cx)
            .into_iter()
            .find(|pending| {
                let serialized = pending.read(cx).serialized();
                serialized.kind == SETTINGS_KIND
                    && serde_json::from_value::<SerializedSettings>(serialized.data.clone())
                        .unwrap_or_default()
                        .defaults
                        == defaults
            });
        if let Some(pending) = pending {
            let id = pending.entity_id();
            self.panes
                .update(cx, |panes, cx| panes.activate_item(id, true, window, cx));
            return self
                .panes
                .read(cx)
                .items_of::<SettingsEditor>(cx)
                .into_iter()
                .find(|editor| editor.read(cx).is_defaults() == defaults);
        }
        let editor = if defaults {
            cx.new(|cx| SettingsEditor::defaults(window, cx))
        } else {
            let Some(file) = self.settings_file.clone() else {
                window.push_notification(
                    Notification::info(format!("{PRODUCT_NAME} runs without a settings file."))
                        .title(PRODUCT_NAME),
                    cx,
                );
                return None;
            };
            cx.new(|cx| SettingsEditor::open(file, window, cx))
        };
        self.add_item(Box::new(editor.clone()), window, cx);
        Some(editor)
    }

    /// A random two-word name for a new draft (`brave-otter`) that no open tab has.
    pub fn new_draft_name(&mut self, cx: &App) -> String {
        let titles: Vec<String> = self
            .panes
            .read(cx)
            .items(cx)
            .iter()
            .map(|item| item.tab_title(cx).to_string())
            .collect();
        ui::names::two_word_name(&titles, &mut self.names)
    }

    /// Writes the layout to `ui_state.json` now (it is otherwise saved a moment after
    /// each change).
    pub fn save_layout(&mut self, cx: &App) {
        self.save_task = None;
        let layout = self.panes.read(cx).serialize(cx);
        self.ui_state.set_layout(layout);
    }

    /// The thread the agent column attaches as context: from the active mail view
    /// (when exactly one row is selected and its message is shown) or message tab.
    pub fn agent_context(&self, cx: &App) -> Option<(ThreadId, SharedString)> {
        if let Some(view) = self.active_mail_view(cx) {
            return view.read(cx).agent_context(cx);
        }
        self.active_message_tab(cx)
            .and_then(|tab| tab.read(cx).agent_context(cx))
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

    /// Re-reads what `changes` affects: the folder tree, the counts, every mail view's
    /// list (keeping the selection) and shown message, and the message tabs. Once the
    /// mail is open (`everything`), tabs that waited for it are restored.
    pub fn apply_changes(
        &mut self,
        changes: &DataChanges,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if changes.is_empty() {
            return;
        }
        if changes.everything || changes.folders {
            self.folder_pane.update(cx, |pane, cx| pane.reload(cx));
        } else if changes.counts {
            self.folder_pane
                .update(cx, |pane, cx| pane.refresh_counts(cx));
        }
        let panes = self.panes.read(cx);
        let views = panes.items_of::<MailView>(cx);
        let tabs = panes.items_of::<MessageTab>(cx);
        for view in views {
            view.update(cx, |view, cx| view.apply_changes(changes, cx));
        }
        for tab in tabs {
            let stale = changes.everything
                || changes.messages_updated.contains(&tab.read(cx).message())
                || changes.threads;
            if stale {
                tab.update(cx, |tab, cx| tab.reload(cx));
            }
        }
        if changes.everything {
            self.panes
                .update(cx, |panes, cx| panes.retry_pending(window, cx));
            self.sync_folder_selection(cx);
        }
        cx.notify();
    }

    // ---- tabs ------------------------------------------------------------------------

    /// Subscribes to the mail views that came and forgets the ones that went.
    fn track_items(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let views = self.panes.read(cx).items_of::<MailView>(cx);
        let open: Vec<EntityId> = views.iter().map(|view| view.entity_id()).collect();
        self.item_subscriptions.retain(|id, _| open.contains(id));
        for view in views {
            self.item_subscriptions
                .entry(view.entity_id())
                .or_insert_with(|| cx.subscribe_in(&view, window, Self::on_mail_view_event));
        }
    }

    fn on_pane_group_event(
        &mut self,
        _: &Entity<PaneGroup>,
        event: &PaneGroupEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            PaneGroupEvent::LayoutChanged => {
                self.track_items(window, cx);
                self.schedule_layout_save(cx);
            }
            PaneGroupEvent::ActiveItemChanged => {
                self.track_items(window, cx);
                self.sync_folder_selection(cx);
            }
        }
        cx.notify();
    }

    fn schedule_layout_save(&mut self, cx: &mut Context<'_, Self>) {
        let timer = cx.background_executor().timer(LAYOUT_SAVE_DELAY);
        self.save_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| this.save_layout(cx)).ok();
        }));
    }

    /// The folder pane shows the folder of the active mail view.
    fn sync_folder_selection(&mut self, cx: &mut Context<'_, Self>) {
        let Some(view) = self.active_mail_view(cx) else {
            return;
        };
        if let ThreadScope::Folder(key) = view.read(cx).scope(cx) {
            self.folder_pane.update(cx, |pane, cx| {
                if pane.selected() != Some(&key) {
                    pane.select(&key, cx);
                }
            });
        }
    }

    fn on_mail_view_event(
        &mut self,
        view: &Entity<MailView>,
        event: &MailViewEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            MailViewEvent::CountsChanged => {
                self.folder_pane
                    .update(cx, |pane, cx| pane.refresh_counts(cx));
            }
            MailViewEvent::SelectionChanged => {}
            MailViewEvent::OpenMessage(message) => {
                let pane = self.panes.read(cx).pane_of(view.entity_id(), cx);
                self.open_message(*message, pane, window, cx);
            }
        }
        cx.notify();
    }

    /// Opens `message` in a tab of its own in `pane` (default: the active pane), or
    /// activates the tab it has there already.
    pub fn open_message(
        &mut self,
        message: MessageId,
        pane: Option<Entity<Pane>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let pane = pane.unwrap_or_else(|| self.panes.read(cx).active_pane().clone());
        let existing = pane.read(cx).items().iter().find_map(|item| {
            item.downcast::<MessageTab>()
                .filter(|tab| tab.read(cx).message() == message)
                .map(|tab| tab.entity_id())
        });
        if let Some(id) = existing {
            self.panes
                .update(cx, |panes, cx| panes.activate_item(id, true, window, cx));
            return;
        }
        let (source, offset) = (self.source.clone(), self.views.local_offset);
        let tab = cx.new(|cx| MessageTab::new(source, offset, message, cx));
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(tab), None, true, true, window, cx);
        });
    }

    /// `enter`, `cmd-o`: the message shown in the active mail view in a tab of its own.
    fn open_selected_message(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(message) = self
            .active_mail_view(cx)
            .and_then(|view| view.read(cx).shown_message(cx))
        else {
            return;
        };
        self.open_message(message, None, window, cx);
    }

    /// Opens a folder in the active mail view, the one used last, or a new one.
    fn open_folder(
        &mut self,
        scope: ThreadScope,
        title: SharedString,
        search: Option<String>,
        new_tab: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<MailView> {
        let target = (!new_tab).then(|| self.current_mail_view(cx)).flatten();
        match target {
            Some(view) => {
                if self.active_mail_view(cx).as_ref() != Some(&view) {
                    let id = view.entity_id();
                    self.panes
                        .update(cx, |panes, cx| panes.activate_item(id, false, window, cx));
                }
                view.update(cx, |view, cx| view.open(scope, title, search, cx));
                view
            }
            None => {
                let view = self.new_mail_view(scope, title, search, window, cx);
                self.panes.update(cx, |panes, cx| {
                    panes.add_item(Box::new(view.clone()), false, window, cx);
                });
                view
            }
        }
    }

    fn on_folder_pane_event(
        &mut self,
        _: &Entity<FolderPane>,
        event: &FolderPaneEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.search
            .update(cx, |search, cx| search.set_value("", window, cx));
        match event {
            FolderPaneEvent::Selected { key, title } => {
                self.open_folder(
                    ThreadScope::Folder(key.clone()),
                    title.clone(),
                    None,
                    false,
                    window,
                    cx,
                );
            }
            FolderPaneEvent::OpenInNewTab { key, title } => {
                let view = self.open_folder(
                    ThreadScope::Folder(key.clone()),
                    title.clone(),
                    None,
                    true,
                    window,
                    cx,
                );
                MailView::focus_list(&view, window, cx);
            }
        }
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
            let view = self.open_folder(
                ThreadScope::AllFolders,
                title,
                Some(query),
                false,
                window,
                cx,
            );
            MailView::focus_list(&view, window, cx);
        }
    }

    // ---- mail actions ---------------------------------------------------------------

    /// Runs `f` on the active mail view's list.
    fn with_list(
        &mut self,
        cx: &mut Context<'_, Self>,
        f: impl FnOnce(&mut ThreadList, &mut Context<'_, ThreadList>),
    ) {
        if let Some(view) = self.active_mail_view(cx) {
            let list = view.read(cx).thread_list().clone();
            list.update(cx, f);
        }
    }

    /// Runs a change on the active message tab, then refreshes the lists.
    fn with_message_tab(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
        f: impl FnOnce(
            &mut MessageTab,
            &mut Context<'_, MessageTab>,
        ) -> Result<(), crate::source::SourceError>,
    ) {
        let Some(tab) = self.active_message_tab(cx) else {
            return;
        };
        let message = tab.read(cx).message();
        if let Err(error) = tab.update(cx, f) {
            tracing::warn!(%error, "changing the message failed");
            window.push_notification(
                Notification::error(format!("The message couldn't be changed: {error}"))
                    .title(PRODUCT_NAME),
                cx,
            );
            return;
        }
        let changes = DataChanges {
            counts: true,
            threads: true,
            messages_updated: vec![message],
            ..DataChanges::default()
        };
        self.apply_changes(&changes, window, cx);
    }

    fn toggle_read(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.active_message_tab(cx).is_some() {
            self.with_message_tab(window, cx, |tab, _| tab.toggle_read());
        } else {
            self.with_list(cx, |list, cx| list.toggle_read(cx));
        }
    }

    fn toggle_star(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.active_message_tab(cx).is_some() {
            self.with_message_tab(window, cx, |tab, _| tab.toggle_star());
        } else {
            self.with_list(cx, |list, cx| list.toggle_star(cx));
        }
    }

    fn archive(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.active_message_tab(cx).is_some() {
            self.with_message_tab(window, cx, |tab, cx| tab.archive(cx));
        } else {
            self.with_list(cx, |list, cx| list.archive_selected(cx));
        }
    }

    fn undo(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.active_message_tab(cx).is_some() {
            let source = self.source.clone();
            self.with_message_tab(window, cx, move |_, _| source.undo().map(drop));
        } else {
            self.with_list(cx, |list, cx| list.undo(cx));
        }
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
            None => self.focus_active_item(window, cx),
        }
        cx.notify();
    }

    fn focus_active_item(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane = self.panes.read(cx).active_pane().clone();
        pane.update(cx, |pane, cx| pane.focus_active_item(window, cx));
    }

    /// `shift-m`: opens Quick Move for the selected rows (or thread) of the active mail
    /// view, or the message of the active message tab, over their account's folders.
    /// Run from the palette, it takes the palette's place.
    fn open_quick_move(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.quick_move.is_some() {
            return;
        }
        let (owner, items, scope, return_to) = if let Some(view) = self.active_mail_view(cx) {
            let (list, message_view) = {
                let view = view.read(cx);
                (view.thread_list().clone(), view.message_view().clone())
            };
            let items = list.update(cx, |list, _| list.command_items());
            let scope = Some(list.read(cx).scope().clone());
            let message = message_view.focus_handle(cx);
            let return_to = if message.is_focused(window) {
                message
            } else {
                list.focus_handle(cx)
            };
            (MoveOwner::List(list), items, scope, return_to)
        } else if let Some(tab) = self.active_message_tab(cx) {
            let items = vec![ItemId::Message(tab.read(cx).message())];
            let return_to = tab.read(cx).focus_handle(cx);
            (MoveOwner::Message(tab), items, None, return_to)
        } else {
            return;
        };
        // Mail moves within its account. The first rows tell (checking a huge selection
        // row by row would stall; the core refuses a move across accounts anyway).
        let mut accounts = items
            .iter()
            .take(ACCOUNT_CHECK_ROWS)
            .map(|item| match *item {
                ItemId::Thread(thread) => self.source.thread_account(thread),
                ItemId::Message(message) => self.source.message_account(message),
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
        let from = scope.and_then(|scope| source_folder(&scope, &account));
        let target = MoveTarget {
            items,
            account: account.id,
            from,
        };
        let recent = self.ui_state.move_targets(account.id);
        let return_focus = match self.palette.take() {
            Some(_) => self.palette_return_focus.take(),
            // From a context menu the focus is on the menu, which is gone by the time
            // the picker closes: back to the message, or else to the list.
            None => Some(return_to),
        };
        self.quick_move_return_focus = return_focus;
        self.quick_move_owner = Some(owner);
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
            let moved = match self.quick_move_owner.clone() {
                Some(MoveOwner::List(list)) => list.update(cx, |list, cx| {
                    list.move_items(&target.items, folder, target.from, cx)
                }),
                Some(MoveOwner::Message(tab)) => {
                    let message = tab.read(cx).message();
                    let moved = self.source.move_messages(&[message], folder, target.from);
                    if moved.is_ok() {
                        let changes = DataChanges {
                            counts: true,
                            threads: true,
                            ..DataChanges::default()
                        };
                        self.apply_changes(&changes, window, cx);
                    }
                    moved
                }
                None => Ok(()),
            };
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
        self.quick_move_owner = None;
        match self.quick_move_return_focus.take() {
            Some(focus) => window.focus(&focus, cx),
            None => self.focus_active_item(window, cx),
        }
        cx.notify();
    }

    /// `mail::ToggleFavoriteFolder`: adds the folder of the active mail view to the
    /// favorites or removes it, as the folder pane's context menu does.
    fn toggle_favorite_folder(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let scope = self
            .active_mail_view(cx)
            .map(|view| view.read(cx).scope(cx));
        let Some(ThreadScope::Folder(key @ FolderKey::Folder(folder))) = scope else {
            // A unified folder, a search, or no mail view.
            window.push_notification(
                Notification::info("Only folders of an account can be favorites.")
                    .title(PRODUCT_NAME),
                cx,
            );
            return;
        };
        self.folder_pane.update(cx, |pane, cx| {
            let favorite = pane.is_favorite(&key);
            pane.set_favorite(folder, !favorite, window, cx);
        });
    }

    fn sort_list(&mut self, by: SortBy, cx: &mut Context<'_, Self>) {
        self.with_list(cx, |list, cx| list.sort_by(by, cx));
    }

    /// Reply, Reply All, Forward: asks the app to open a draft for the message of the
    /// active tab (the newest of a selected thread, the selected message, or the
    /// message tab's).
    fn request_compose(
        &mut self,
        request: impl FnOnce(MessageId) -> ComposeRequest,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let shown = match self.active_message_tab(cx) {
            Some(tab) => Some(tab.read(cx).message()),
            None => self
                .active_mail_view(cx)
                .and_then(|view| view.read(cx).shown_message(cx)),
        };
        match shown {
            Some(message) => cx.emit(request(message)),
            None => window.push_notification(
                Notification::info("Select one message first.").title(PRODUCT_NAME),
                cx,
            ),
        }
    }

    fn on_column_drag(
        &mut self,
        event: &DragMoveEvent<ColumnDrag>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let x = f32::from(event.event.position.x);
        let width = f32::from(window.viewport_size().width);
        let mut prefs = self.prefs.borrow_mut();
        match event.drag(cx) {
            ColumnDrag::FolderPane => {
                prefs.folder_pane = (x - self.config.spaces_bar).max(self.config.min_folder_pane);
            }
            ColumnDrag::Agent => {
                prefs.agent = (width - x).max(self.config.min_agent);
            }
        }
        drop(prefs);
        cx.notify();
    }

    fn effective_agent_expanded(&self, window: &Window) -> bool {
        let width = f32::from(window.viewport_size().width);
        layout::agent_expanded(width, &self.prefs.borrow(), &self.config)
    }

    // ---- benchmark ------------------------------------------------------------------

    /// Advances the scroll benchmark by one frame.
    fn step_benchmark(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.bench.is_none() {
            return;
        }
        let list = self.thread_list(cx);
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
        if let Some(list) = list {
            let list = list.read(cx);
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
        let view = self.open_folder(ThreadScope::Folder(key), title, None, false, window, cx);
        let list = view.read(cx).thread_list().clone();
        list.update(cx, |list, cx| {
            if config.table != (list.mode() == ListMode::Table) {
                list.toggle_mode(window, cx);
            }
        });
        tracing::info!(target: "mazemail::perf", ?config, "starting scroll benchmark");
        self.bench = Some(BenchRun { config, frame: 0 });
        window.request_animation_frame();
    }

    // ---- rendering ------------------------------------------------------------------

    fn render_title_bar(&self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let folders_visible = self.prefs.borrow().folder_pane_visible;
        TitleBar::new()
            .bg(theme.title_bar)
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .w_full()
                    .pr_2()
                    .gap_1()
                    .child(
                        div().debug_selector(|| "toggle-folder-pane".into()).child(
                            Button::new("toggle-folder-pane")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(if folders_visible {
                                    IconName::PanelLeftClose
                                } else {
                                    IconName::PanelLeftOpen
                                }))
                                .tooltip_with_action(
                                    if folders_visible {
                                        "Hide Folder Pane"
                                    } else {
                                        "Show Folder Pane"
                                    },
                                    &ToggleFolderPane,
                                    Some("Workspace"),
                                )
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(ToggleFolderPane), cx);
                                }),
                        ),
                    )
                    .child(
                        div().debug_selector(|| "new-message".into()).child(
                            Button::new("new-message")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Plus))
                                .tooltip_with_action("New Message", &NewMessage, Some("Workspace"))
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(NewMessage), cx);
                                }),
                        ),
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

    fn register_actions(
        &self,
        root: gpui::Stateful<gpui::Div>,
        cx: &mut Context<'_, Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let root = ui::panes::bind_actions(root, &self.panes);
        root.on_action(cx.listener(|this, _: &ToggleFolderPane, _, cx| {
            let mut prefs = this.prefs.borrow_mut();
            prefs.folder_pane_visible = !prefs.folder_pane_visible;
            drop(prefs);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ToggleAgentColumn, window, cx| {
            let expanded = this.effective_agent_expanded(window);
            this.prefs.borrow_mut().agent_expanded = Some(!expanded);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &ToggleMessagePane, _, cx| {
            let mut prefs = this.prefs.borrow_mut();
            prefs.message_visible = !prefs.message_visible;
            drop(prefs);
            cx.notify();
        }))
        .on_action(cx.listener(|this, _: &Toggle, window, cx| this.toggle_palette(window, cx)))
        .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
            let focus = this.search.focus_handle(cx);
            window.focus(&focus, cx);
        }))
        .on_action(cx.listener(|this, _: &QuickFilter, window, cx| {
            if let Some(list) = this
                .active_mail_view(cx)
                .map(|v| v.read(cx).thread_list().clone())
            {
                list.update(cx, |list, cx| list.focus_quick_filter(window, cx));
            }
        }))
        .on_action(cx.listener(|_, _: &NewMessage, _, cx| {
            cx.emit(ComposeRequest::New);
        }))
        .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
            this.open_settings(false, window, cx);
        }))
        .on_action(cx.listener(|this, _: &OpenDefaultSettings, window, cx| {
            this.open_settings(true, window, cx);
        }))
        .on_action(cx.listener(|this, _: &OpenMessage, window, cx| {
            this.open_selected_message(window, cx);
        }))
        .on_action(cx.listener(|this, _: &OpenFolderInNewTab, window, cx| {
            let (key, title) = {
                let pane = this.folder_pane.read(cx);
                let key = pane
                    .selected()
                    .cloned()
                    .unwrap_or(FolderKey::Unified(FolderRole::Inbox));
                let title = folder_title(pane, &key).unwrap_or_else(|| "Inbox".into());
                (key, title)
            };
            let view = this.open_folder(ThreadScope::Folder(key), title, None, true, window, cx);
            MailView::focus_list(&view, window, cx);
        }))
        .on_action(cx.listener(|this, _: &NextMessage, _, cx| {
            this.with_list(cx, |list, cx| list.next(cx));
        }))
        .on_action(cx.listener(|this, _: &PreviousMessage, _, cx| {
            this.with_list(cx, |list, cx| list.previous(cx));
        }))
        .on_action(cx.listener(|this, _: &NextUnread, _, cx| {
            this.with_list(cx, |list, cx| {
                list.jump_unread(Direction::Forward, cx);
            });
        }))
        .on_action(cx.listener(|this, _: &PreviousUnread, _, cx| {
            this.with_list(cx, |list, cx| {
                list.jump_unread(Direction::Backward, cx);
            });
        }))
        .on_action(cx.listener(|this, _: &FirstMessage, _, cx| {
            this.with_list(cx, |list, cx| list.edge(false, cx));
        }))
        .on_action(cx.listener(|this, _: &LastMessage, _, cx| {
            this.with_list(cx, |list, cx| list.edge(true, cx));
        }))
        .on_action(cx.listener(|this, _: &PageDown, _, cx| {
            this.with_list(cx, |list, cx| list.page(true, cx));
        }))
        .on_action(cx.listener(|this, _: &PageUp, _, cx| {
            this.with_list(cx, |list, cx| list.page(false, cx));
        }))
        .on_action(cx.listener(|this, _: &ToggleRead, window, cx| this.toggle_read(window, cx)))
        .on_action(cx.listener(|this, _: &ToggleStar, window, cx| this.toggle_star(window, cx)))
        .on_action(cx.listener(|this, _: &Archive, window, cx| this.archive(window, cx)))
        .on_action(cx.listener(|this, _: &Undo, window, cx| this.undo(window, cx)))
        .on_action(cx.listener(|this, _: &MoveToFolder, window, cx| {
            this.open_quick_move(window, cx);
        }))
        .on_action(cx.listener(|this, _: &ToggleFavoriteFolder, window, cx| {
            this.toggle_favorite_folder(window, cx);
        }))
        .on_action(cx.listener(|this, _: &ToggleTableView, window, cx| {
            this.with_list(cx, |list, cx| list.toggle_mode(window, cx));
        }))
        .on_action(cx.listener(|this, _: &ToggleThreads, _, cx| {
            this.with_list(cx, |list, cx| list.toggle_threads(cx));
        }))
        .on_action(cx.listener(|this, _: &SortByDate, _, cx| this.sort_list(SortBy::Date, cx)))
        .on_action(cx.listener(|this, _: &SortByReceived, _, cx| {
            this.sort_list(SortBy::Received, cx);
        }))
        .on_action(cx.listener(|this, _: &SortByStar, _, cx| this.sort_list(SortBy::Star, cx)))
        .on_action(cx.listener(|this, _: &SortByUnread, _, cx| {
            this.sort_list(SortBy::Unread, cx);
        }))
        .on_action(cx.listener(|this, _: &SortByAttachment, _, cx| {
            this.sort_list(SortBy::Attachment, cx);
        }))
        .on_action(cx.listener(|this, _: &SortBySubject, _, cx| {
            this.sort_list(SortBy::Subject, cx);
        }))
        .on_action(cx.listener(|this, _: &SortByCorrespondents, _, cx| {
            this.sort_list(SortBy::Correspondents, cx);
        }))
        .on_action(cx.listener(|this, _: &SortBySender, _, cx| {
            this.sort_list(SortBy::Sender, cx);
        }))
        .on_action(cx.listener(|this, _: &SortByRecipients, _, cx| {
            this.sort_list(SortBy::Recipients, cx);
        }))
        .on_action(cx.listener(|this, _: &SortByTags, _, cx| this.sort_list(SortBy::Tags, cx)))
        .on_action(cx.listener(|this, _: &SortByAccount, _, cx| {
            this.sort_list(SortBy::Account, cx);
        }))
        .on_action(cx.listener(|this, _: &SortBySize, _, cx| this.sort_list(SortBy::Size, cx)))
        .on_action(cx.listener(|this, _: &ReverseSortOrder, _, cx| {
            this.with_list(cx, |list, cx| list.reverse_sort(cx));
        }))
        .on_action(cx.listener(|this, _: &ExpandThread, _, cx| {
            this.with_list(cx, |list, cx| list.expand_selected(cx));
        }))
        .on_action(cx.listener(|this, _: &CollapseThread, _, cx| {
            this.with_list(cx, |list, cx| list.collapse_selected(cx));
        }))
        .on_action(cx.listener(|this, _: &ExpandAllThreads, _, cx| {
            this.with_list(cx, |list, cx| list.expand_all(cx));
        }))
        .on_action(cx.listener(|this, _: &CollapseAllThreads, _, cx| {
            this.with_list(cx, |list, cx| list.collapse_all(cx));
        }))
        .on_action(cx.listener(|this, _: &SelectThread, _, cx| {
            this.with_list(cx, |list, cx| list.select_thread(cx));
        }))
        .on_action(cx.listener(|this, _: &ExtendSelectionDown, _, cx| {
            this.with_list(cx, |list, cx| list.extend(1, cx));
        }))
        .on_action(cx.listener(|this, _: &ExtendSelectionUp, _, cx| {
            this.with_list(cx, |list, cx| list.extend(-1, cx));
        }))
        .on_action(cx.listener(|this, _: &SelectAll, _, cx| {
            this.with_list(cx, |list, cx| list.select_all(cx));
        }))
        .on_action(cx.listener(|this, _: &Reply, window, cx| {
            this.request_compose(
                |message| ComposeRequest::Reply {
                    message,
                    all: false,
                },
                window,
                cx,
            );
        }))
        .on_action(cx.listener(|this, _: &ReplyAll, window, cx| {
            this.request_compose(
                |message| ComposeRequest::Reply { message, all: true },
                window,
                cx,
            );
        }))
        .on_action(cx.listener(|this, _: &Forward, window, cx| {
            this.request_compose(|message| ComposeRequest::Forward { message }, window, cx);
        }))
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
        let prefs = *self.prefs.borrow();
        let resolved = layout::resolve(width, &prefs, &self.config);
        let theme = cx.theme().colors;
        let list = self
            .active_mail_view(cx)
            .map(|view| view.read(cx).thread_list().clone());
        let (summary, has_rows): (SharedString, bool) = match &list {
            Some(list) => {
                let list = list.read(cx);
                let len = list.key_count();
                let noun = if list.threaded() {
                    "threads"
                } else {
                    "messages"
                };
                (
                    format!("{} {noun}", format_thousands(len as u64)).into(),
                    len > 0,
                )
            }
            None => (SharedString::default(), false),
        };
        let context = self.agent_context(cx).map(|(_, subject)| subject);

        let center = div().flex_1().min_w_0().h_full().child(self.panes.clone());
        let agent = match resolved.agent {
            AgentPlacement::Expanded(width) => match &self.agent_panel {
                Some(panel) => Self::column(
                    "resize-agent",
                    width,
                    ColumnDrag::Agent,
                    HandleEdge::Leading,
                    div()
                        .size_full()
                        .border_l_1()
                        .border_color(theme.border)
                        .child(panel.clone()),
                )
                .into_any_element(),
                None => Self::column(
                    "resize-agent",
                    width,
                    ColumnDrag::Agent,
                    HandleEdge::Leading,
                    AgentColumn::expanded(width, context),
                )
                .into_any_element(),
            },
            AgentPlacement::Strip => AgentColumn::strip(self.config.agent_strip).into_any_element(),
        };

        let stats = self.stats.clone();
        let after_paint = self.after_paint.clone();
        let root = v_flex()
            .id("mail-window")
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_drag_move(cx.listener(Self::on_column_drag));
        self.register_actions(root, cx)
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
    use crate::source::ListSort;
    use crate::thread_list::TABLE_LIST_WIDTH;

    pub(crate) fn open(cx: &mut TestAppContext) -> (Entity<MailWindow>, &mut VisualTestContext) {
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let source: Arc<dyn MailSource> =
            Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
        let (view, cx) = cx.add_window_view(|window, cx| {
            MailWindow::new(source, &MailWindowOptions::default(), window, cx)
        });
        // Focus events (the active pane follows the focus) need an active window.
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        (view, cx)
    }

    #[gpui::test]
    fn keymap_drives_selection_and_layout(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let selected = |cx: &mut VisualTestContext| {
            view.read_with(cx, |w, cx| {
                w.thread_list(cx).unwrap().read(cx).selected_index()
            })
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
            w.message_view(cx)
                .unwrap()
                .read(cx)
                .document()
                .map(|d| d.thread)
        });
        let expected = view.read_with(cx, |w, cx| {
            w.thread_list(cx).unwrap().read(cx).selected_thread()
        });
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
        let list = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap());
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
                w.message_view(cx)
                    .unwrap()
                    .read(cx)
                    .document()
                    .map(|d| d.message),
                w.thread_list(cx).unwrap().read(cx).selected_item(),
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
            assert_eq!(w.thread_list(cx).unwrap().read(cx).mode(), ListMode::Table);
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
        let list = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap().focus_handle(cx));
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
        let list = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap().focus_handle(cx));
        cx.update(|window, cx| window.focus(&list, cx));
        let rows = |cx: &mut gpui::VisualTestContext| {
            view.read_with(cx, |w, cx| w.thread_list(cx).unwrap().read(cx).len())
        };
        let before = rows(cx);
        let first = view.read_with(cx, |w, cx| {
            w.thread_list(cx).unwrap().read(cx).selected_thread()
        });
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
        let top = view.read_with(cx, |w, cx| {
            w.thread_list(cx).unwrap().read(cx).selected_thread()
        });
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
            view.read_with(cx, |w, cx| w
                .thread_list(cx)
                .unwrap()
                .read(cx)
                .selected_index()),
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
            view.read_with(cx, |w, cx| {
                w.thread_list(cx).unwrap().read(cx).selected_thread()
            })
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
                w.thread_list(cx)
                    .unwrap()
                    .update(cx, |l, _| l.command_items())
            })
        };
        let first = items(cx);
        cx.simulate_keystrokes("shift-down shift-down shift-up");
        let selected = items(cx);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0], first[0]);
        assert_eq!(
            view.read_with(cx, |w, cx| w.message_view(cx).unwrap().read(cx).selection()),
            Some(2),
            "the message view says how many are selected"
        );
        // A plain arrow collapses; cmd-a selects everything.
        cx.simulate_keystrokes("down");
        assert_eq!(items(cx).len(), 1);
        cx.simulate_keystrokes("cmd-a");
        let all = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap().read(cx).len());
        assert_eq!(
            view.read_with(cx, |w, cx| w
                .thread_list(cx)
                .unwrap()
                .read(cx)
                .selected_count()),
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
            view.read_with(cx, |w, cx| w.message_view(cx).unwrap().read(cx).selection()),
            None
        );

        // The table extends the same way; its own up/down collapse.
        cx.simulate_keystrokes("cmd-alt-t shift-down shift-down");
        assert_eq!(items(cx).len(), 3);
        cx.simulate_keystrokes("down");
        cx.run_until_parked();
        assert_eq!(items(cx).len(), 1);
    }

    /// Right-clicks at the middle of what `selector` marks, and draws the menu.
    fn right_click(cx: &mut VisualTestContext, selector: String) {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is drawn"));
        let modifiers = gpui::Modifiers::none();
        cx.simulate_mouse_down(bounds.center(), gpui::MouseButton::Right, modifiers);
        cx.simulate_mouse_up(bounds.center(), gpui::MouseButton::Right, modifiers);
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// Menu items: Reply, Reply All, Forward, —, Mark as Read/Unread, Star/Unstar, …
    const MARK_READ: &str = "down down down down enter";
    const STAR: &str = "down down down down down enter";

    #[gpui::test]
    fn right_click_selects_a_row_and_its_menu_acts_on_the_selection(cx: &mut TestAppContext) {
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
        let list = view.read_with(cx, |w, cx| w.thread_list(cx).unwrap());
        let threads = |cx: &mut VisualTestContext| {
            list.read_with(cx, |list, _| {
                let crate::thread_list::ListKeys::Threads(keys) =
                    list.model().borrow().keys().clone()
                else {
                    panic!("threaded");
                };
                keys
            })
        };
        let keys = threads(cx);
        let rows = fake.thread_rows(&keys[..12]);
        let ix = (1..12)
            .find(|ix| rows[*ix].is_unread() && !rows[*ix].is_starred())
            .expect("an unread thread near the top");
        let thread = keys[ix];

        // Cards: a right-click selects the row (and shows it) without marking it read.
        right_click(cx, format!("card-row-{ix}"));
        assert_eq!(
            list.read_with(cx, |l, _| l.selected_index()),
            Some(ix),
            "selected"
        );
        assert_eq!(
            view.read_with(cx, |w, cx| w
                .message_view(cx)
                .unwrap()
                .read(cx)
                .document()
                .map(|d| d.thread)),
            Some(thread)
        );
        assert!(fake.thread_rows(&[thread])[0].is_unread());
        cx.simulate_keystrokes(MARK_READ);
        cx.run_until_parked();
        assert!(!fake.thread_rows(&[thread])[0].is_unread(), "Mark as Read");
        right_click(cx, format!("card-row-{ix}"));
        cx.simulate_keystrokes(STAR);
        cx.run_until_parked();
        assert!(fake.thread_rows(&[thread])[0].is_starred(), "Star");
        // The list has the focus back: single-key shortcuts work.
        cx.simulate_keystrokes("s");
        assert!(!fake.thread_rows(&[thread])[0].is_starred());

        // Table: a right-click inside a selection keeps it; the menu acts on all of it.
        cx.simulate_keystrokes("cmd-alt-t");
        cx.run_until_parked();
        list.update(cx, |list, cx| {
            list.select(2, cx);
            list.extend(2, cx);
        });
        cx.run_until_parked();
        let selected: Vec<_> = keys[2..5].to_vec();
        let starred = fake.thread_rows(&selected[2..])[0].is_starred();
        right_click(cx, "table-row-3".to_owned());
        assert_eq!(list.read_with(cx, |l, _| l.selected_count()), 3);
        cx.simulate_keystrokes(STAR);
        cx.run_until_parked();
        assert!(
            fake.thread_rows(&selected)
                .iter()
                .all(|row| row.is_starred() != starred),
            "the cursor row decides, all three follow"
        );
        // An unselected table row is selected first.
        right_click(cx, "table-row-8".to_owned());
        assert_eq!(list.read_with(cx, |l, _| l.selected_count()), 1);
        assert_eq!(list.read_with(cx, |l, _| l.selected_index()), Some(8));
        cx.simulate_keystrokes("escape");

        // The message view offers the same menu for the message it shows.
        let shown = keys[8];
        let unread = fake.thread_rows(&[shown])[0].is_unread();
        right_click(cx, "message-view".to_owned());
        cx.simulate_keystrokes(MARK_READ);
        cx.run_until_parked();
        assert_ne!(fake.thread_rows(&[shown])[0].is_unread(), unread);
    }

    #[gpui::test]
    fn the_palette_sorts_the_list(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("sort by subject");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        let sort = |cx: &mut VisualTestContext| {
            view.read_with(cx, |w, cx| w.thread_list(cx).unwrap().read(cx).sort())
        };
        assert_eq!(sort(cx), ListSort::by(SortBy::Subject));
        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("reverse sort order");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert!(sort(cx).descending);
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
            assert_eq!(w.thread_list(cx).unwrap().read(cx).len(), 1_000_000);
            assert_eq!(
                w.thread_list(cx).unwrap().read(cx).scope(),
                &ThreadScope::Folder(key)
            );
        });
    }
    /// A favorite added from the palette shows under Favorites; clicking it opens the
    /// folder like its row in the account does.
    #[gpui::test]
    fn favorites_are_toggled_from_the_palette_and_open_their_folder(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let pane = view.read_with(cx, |w, _| w.folder_pane.clone());
        let favorites = |cx: &mut VisualTestContext| {
            pane.read_with(cx, |pane, cx| {
                pane.favorite_rows(cx)
                    .into_iter()
                    .map(|(_, label)| label.to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            favorites(cx),
            [
                "Gmail (personal)",
                "Receipts",
                "Microsoft 365",
                "Acme",
                "To Do"
            ]
        );

        // Gmail (club) / Inbox (folder 9): open it, then toggle it from the palette.
        let club_inbox = FolderKey::Folder(mazemail_types::FolderId(9));
        view.update_in(cx, |w, window, cx| {
            let pane = w.folder_pane.clone();
            let selected = FolderPaneEvent::Selected {
                key: club_inbox.clone(),
                title: "Inbox · Gmail (club)".into(),
            };
            w.on_folder_pane_event(&pane, &selected, window, cx);
        });
        let run_toggle = |cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("cmd-shift-p");
            cx.simulate_input("toggle favorite folder");
            cx.simulate_keystrokes("enter");
            cx.run_until_parked();
        };
        run_toggle(cx);
        assert_eq!(
            favorites(cx),
            [
                "Gmail (personal)",
                "Receipts",
                "Gmail (club)",
                "Inbox",
                "Microsoft 365",
                "Acme",
                "To Do"
            ]
        );

        // Back to the unified Inbox, then click the new favorite.
        view.update_in(cx, |w, window, cx| {
            let pane = w.folder_pane.clone();
            let selected = FolderPaneEvent::Selected {
                key: FolderKey::Unified(FolderRole::Inbox),
                title: "Inbox · Unified".into(),
            };
            w.on_folder_pane_event(&pane, &selected, window, cx);
            w.folder_pane.update(cx, |p, cx| {
                p.select(&FolderKey::Unified(FolderRole::Inbox), cx)
            });
        });
        // `debug_bounds` wants a `&'static str`.
        let row: &'static str = format!("folder-pane-row:favorite:{}", club_inbox.tree_id()).leak();
        cx.run_until_parked();
        let bounds = cx.debug_bounds(row).expect("the favorite has a row");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |w, cx| {
            let list = w.thread_list(cx).unwrap().read(cx);
            assert_eq!(list.scope(), &ThreadScope::Folder(club_inbox.clone()));
        });

        // Toggling again removes it. (On a unified folder the action only explains, in
        // a notification, which needs the real window root.)
        run_toggle(cx);
        assert!(!favorites(cx).contains(&"Gmail (club)".to_owned()));
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

    /// The title bar's "+" and `cmd-n` ask the app for a new draft; Reply, Reply All and
    /// Forward ask it to compose for the shown message.
    #[gpui::test]
    fn new_message_and_replies_reach_the_app(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.run_until_parked();
        let requests: Rc<RefCell<Vec<ComposeRequest>>> = Rc::default();
        let seen = requests.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, move |_, request: &ComposeRequest, _| {
                seen.borrow_mut().push(*request);
            })
        });
        let bounds = cx.debug_bounds("new-message").expect("the + button");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes("cmd-n");
        assert_eq!(
            *requests.borrow(),
            [ComposeRequest::New, ComposeRequest::New],
            "the + button and cmd-n"
        );
        requests.borrow_mut().clear();

        let shown = view
            .read_with(cx, |w, cx| {
                w.message_view(cx)
                    .unwrap()
                    .read(cx)
                    .document()
                    .map(|d| d.message)
            })
            .expect("a message is shown");
        cx.simulate_keystrokes("cmd-r cmd-shift-r cmd-l");
        cx.run_until_parked();
        assert_eq!(
            *requests.borrow(),
            vec![
                ComposeRequest::Reply {
                    message: shown,
                    all: false
                },
                ComposeRequest::Reply {
                    message: shown,
                    all: true
                },
                ComposeRequest::Forward { message: shown },
            ]
        );

        // In the list, `r` replies and `shift-r` replies to all, without cmd.
        requests.borrow_mut().clear();
        let list = view
            .read_with(cx, |w, cx| w.thread_list(cx))
            .expect("a mail view")
            .read_with(cx, |list, cx| list.focus_handle(cx));
        cx.update(|window, cx| window.focus(&list, cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("r shift-r");
        cx.run_until_parked();
        assert_eq!(
            *requests.borrow(),
            vec![
                ComposeRequest::Reply {
                    message: shown,
                    all: false
                },
                ComposeRequest::Reply {
                    message: shown,
                    all: true
                },
            ]
        );
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

    mod panes;
    mod settings_tab;
}

//! A mail view: the message list and the message of one folder (or of search results),
//! the main kind of tab in the window's center (ADR-018). Several can be open, each
//! with its own list and selection; the list's view (cards or table, columns, sort)
//! and the list width are the same in all of them.
//!
//! The list and the message sit side by side while the view is wide enough for both
//! at their minimum widths, else the message moves below the list
//! ([`layout::resolve_mail_view`]); the view measures its own width for that, so a
//! narrow split stacks while a wide one doesn't.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Axis, Context, DragMoveEvent, Empty, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, canvas, div, px, relative,
};
use gpui_kit::component::{
    ActiveTheme as _, h_flex,
    resizable::{HandleEdge, resize_handle},
    v_flex,
};
use mazemail_types::{FolderId, FolderRole, MessageId, ThreadId};
use serde::{Deserialize, Serialize};
use time::UtcOffset;
use ui::assets::IconName;
use ui::panes::{Item, ItemEvent, SerializedItem};

use crate::actions::ToggleOriginalHtml;
use crate::layout::{self, LayoutConfig, LayoutPrefs, MessagePlacement};
use crate::message_view::{MessageView, ViewPlace};
use crate::source::{DataChanges, FolderKey, MailSource, ThreadScope};
use crate::thread_list::{
    ItemId, ListMode, TABLE_LIST_WIDTH, ThreadList, ThreadListEvent, message_menu,
};
use crate::ui_state::UiStateStore;

/// The layout kind of mail views (`{"kind": "mail", "data": {"type": "unified", …}}`).
pub const MAIL_KIND: &str = "mail";

/// What every mail view of a window shares.
#[derive(Clone)]
pub struct MailViewContext {
    /// The mail.
    pub source: Arc<dyn MailSource>,
    /// The user's UTC offset, for dates.
    pub local_offset: UtcOffset,
    /// The list's view, remembered across restarts.
    pub ui_state: UiStateStore,
    /// The window's column widths and toggles (the list width, `F8`).
    pub prefs: Rc<RefCell<LayoutPrefs>>,
    /// Fixed sizes and thresholds.
    pub config: LayoutConfig,
}

impl std::fmt::Debug for MailViewContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailViewContext").finish_non_exhaustive()
    }
}

/// What a mail view shows, as the layout file keeps it: folder ids, never names.
/// Search results are kept as the unified Inbox (the query isn't written to disk).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SerializedFolder {
    /// One folder of one account.
    Folder {
        /// The folder.
        id: FolderId,
    },
    /// A unified folder.
    Unified {
        /// Its role.
        role: FolderRole,
    },
}

impl SerializedFolder {
    /// The folder key.
    pub fn key(self) -> FolderKey {
        match self {
            Self::Folder { id } => FolderKey::Folder(id),
            Self::Unified { role } => FolderKey::Unified(role),
        }
    }

    /// What `scope` is kept as.
    pub fn of(scope: &ThreadScope) -> Self {
        match scope {
            ThreadScope::Folder(FolderKey::Folder(id)) => Self::Folder { id: *id },
            ThreadScope::Folder(FolderKey::Unified(role)) => Self::Unified { role: *role },
            ThreadScope::AllFolders => Self::Unified {
                role: FolderRole::Inbox,
            },
        }
    }
}

/// What a mail view reports to the window.
#[derive(Debug, Clone, PartialEq)]
pub enum MailViewEvent {
    /// Flags changed or threads moved: the folder counts need a refresh.
    CountsChanged,
    /// The selection, and with it the shown message, changed.
    SelectionChanged,
    /// Open this message in a tab of its own (a double-click on its row).
    OpenMessage(MessageId),
}

/// The list column being resized; also the drag payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListDrag;

impl Render for ListDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        Empty
    }
}

/// The message list and the message view of one folder.
pub struct MailView {
    context: MailViewContext,
    thread_list: Entity<ThreadList>,
    message_view: Entity<MessageView>,
    /// The list width before the table needed more room.
    cards_list_width: Option<f32>,
    /// The width the view had when it was drawn last.
    width: Rc<Cell<Option<f32>>>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for MailView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailView").finish_non_exhaustive()
    }
}

impl EventEmitter<MailViewEvent> for MailView {}
impl EventEmitter<ItemEvent> for MailView {}

impl MailView {
    /// A view of `scope`, titled `title`.
    pub fn new(
        context: MailViewContext,
        scope: ThreadScope,
        title: SharedString,
        search: Option<String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let message_view = cx.new(|cx| {
            MessageView::new(
                Some(context.source.clone()),
                context.local_offset,
                ViewPlace::Pane,
                cx,
            )
        });
        let thread_list = cx.new(|cx| {
            ThreadList::new(
                context.source.clone(),
                context.local_offset,
                scope.clone(),
                title.clone(),
                context.ui_state.clone(),
                window,
                cx,
            )
        });
        if search.is_some() {
            thread_list.update(cx, |list, cx| list.open(scope, title, search, cx));
        }
        let menu = message_menu(thread_list.downgrade());
        message_view.update(cx, |view, _| view.set_context_menu(Rc::new(menu)));
        let subscriptions = vec![cx.subscribe_in(&thread_list, window, Self::on_thread_list_event)];
        let mut this = Self {
            context,
            thread_list,
            message_view,
            cards_list_width: None,
            width: Rc::default(),
            _subscriptions: subscriptions,
        };
        if this.thread_list.read(cx).mode() == ListMode::Table {
            // The list remembered the table view.
            this.fit_list_to_table(cx);
        }
        this.show_selected(cx);
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

    /// What the list shows.
    pub fn scope(&self, cx: &App) -> ThreadScope {
        self.thread_list.read(cx).scope().clone()
    }

    /// Opens a folder (or search results) in this view.
    pub fn open(
        &mut self,
        scope: ThreadScope,
        title: SharedString,
        search: Option<String>,
        cx: &mut Context<'_, Self>,
    ) {
        self.thread_list
            .update(cx, |list, cx| list.open(scope, title, search, cx));
        cx.emit(ItemEvent::UpdateTab);
        cx.emit(ItemEvent::Edited);
        cx.notify();
    }

    /// Focuses the list of `view`.
    pub fn focus_list(view: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let list = view.read(cx).thread_list.clone();
        list.update(cx, |list, cx| list.focus(window, cx));
    }

    /// The message shown for a single selected row (the newest of a selected thread).
    pub fn shown_message(&self, cx: &App) -> Option<MessageId> {
        (self.thread_list.read(cx).selected_count() == 1)
            .then(|| self.message_view.read(cx).document().map(|doc| doc.message))
            .flatten()
    }

    /// The thread the agent column attaches: the selected thread and its subject, when
    /// exactly one row is selected and its message is shown.
    pub fn agent_context(&self, cx: &App) -> Option<(ThreadId, SharedString)> {
        if self.thread_list.read(cx).selected_count() > 1 {
            return None;
        }
        self.message_view
            .read(cx)
            .document()
            .map(|doc| (doc.thread, doc.subject.expose().clone()))
    }

    /// Re-reads what `changes` affects: the list's keys (keeping the selection) and the
    /// displayed message.
    pub fn apply_changes(&mut self, changes: &DataChanges, cx: &mut Context<'_, Self>) {
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
        let source = self.context.source.clone();
        let document = item.map(|item| match item {
            ItemId::Thread(thread) => source.message(thread),
            ItemId::Message(message) => source.message_document(message),
        });
        self.message_view.update(cx, |view, cx| match document {
            None => view.show(None, cx),
            Some(Ok(document)) => view.show(Some(document), cx),
            Some(Err(error)) => view.show_error(error.to_string(), cx),
        });
    }

    /// The message of a list row: itself, or the newest of a thread.
    fn message_of(&self, item: ItemId, cx: &App) -> Option<MessageId> {
        match item {
            ItemId::Message(message) => Some(message),
            ItemId::Thread(thread) => {
                let shown = self
                    .message_view
                    .read(cx)
                    .document()
                    .filter(|doc| doc.thread == thread)
                    .map(|doc| doc.message);
                shown.or_else(|| self.context.source.message(thread).ok().map(|d| d.message))
            }
        }
    }

    fn on_thread_list_event(
        &mut self,
        _: &Entity<ThreadList>,
        event: &ThreadListEvent,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            ThreadListEvent::Selected(_) => {
                self.show_selected(cx);
                cx.emit(MailViewEvent::SelectionChanged);
            }
            ThreadListEvent::Changed => cx.emit(MailViewEvent::CountsChanged),
            ThreadListEvent::ModeChanged(ListMode::Table) => self.fit_list_to_table(cx),
            ThreadListEvent::ModeChanged(ListMode::Cards) => {
                if let Some(width) = self.cards_list_width.take() {
                    self.context.prefs.borrow_mut().list = width;
                }
            }
            ThreadListEvent::ColumnsChanged => {
                if self.thread_list.read(cx).mode() == ListMode::Table {
                    let needed = self.thread_list.read(cx).table_width();
                    let mut prefs = self.context.prefs.borrow_mut();
                    prefs.list = prefs.list.max(needed);
                }
            }
            ThreadListEvent::Open(item) => {
                if let Some(message) = self.message_of(*item, cx) {
                    cx.emit(MailViewEvent::OpenMessage(message));
                }
            }
        }
        cx.notify();
    }

    /// The table needs room for its columns; remembers the card width.
    fn fit_list_to_table(&mut self, cx: &App) {
        let mut prefs = self.context.prefs.borrow_mut();
        self.cards_list_width = Some(prefs.list);
        let needed = self.thread_list.read(cx).table_width();
        prefs.list = prefs.list.max(needed.max(TABLE_LIST_WIDTH));
    }

    fn on_list_drag(
        &mut self,
        event: &DragMoveEvent<ListDrag>,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let x = f32::from(event.event.position.x - event.bounds.left());
        let min = self.context.config.min_list;
        self.context.prefs.borrow_mut().list = x.max(min);
        cx.notify();
    }
}

impl Focusable for MailView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.thread_list.focus_handle(cx)
    }
}

impl Render for MailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let width = self
            .width
            .get()
            .unwrap_or_else(|| f32::from(window.viewport_size().width));
        let resolved = {
            let prefs = self.context.prefs.borrow();
            layout::resolve_mail_view(width, &prefs, &self.context.config)
        };
        let list = self.thread_list.clone();
        let message = div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(self.message_view.clone());
        let body = match resolved.message {
            MessagePlacement::Beside => h_flex()
                .size_full()
                .child(
                    div()
                        .relative()
                        .h_full()
                        .w(px(resolved.list.unwrap_or(self.context.config.min_list)))
                        .flex_none()
                        .child(
                            div()
                                .size_full()
                                .border_r_1()
                                .border_color(theme.border)
                                .child(list),
                        )
                        .child(
                            resize_handle::<ListDrag, ListDrag>("resize-list", Axis::Horizontal)
                                .inside(HandleEdge::Trailing)
                                .on_drag(ListDrag, |drag, _, _, cx| cx.new(|_| *drag)),
                        ),
                )
                .child(message.h_full())
                .into_any_element(),
            MessagePlacement::Below => v_flex()
                .size_full()
                .child(
                    div()
                        .h(relative(0.45))
                        .w_full()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(list),
                )
                .child(message.w_full())
                .into_any_element(),
            MessagePlacement::Hidden => div().size_full().child(list).into_any_element(),
        };
        let measured = self.width.clone();
        let this = cx.weak_entity();
        div()
            .id("mail-view")
            .relative()
            .size_full()
            .on_drag_move(cx.listener(Self::on_list_drag))
            // From the list: switch the message next to it.
            .on_action(cx.listener(|this, _: &ToggleOriginalHtml, _, cx| {
                this.message_view
                    .update(cx, |view, cx| view.toggle_mode(cx));
            }))
            .child(body)
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let width = f32::from(bounds.size.width);
                        let changed = measured.get().is_none_or(|old| (old - width).abs() > 0.5);
                        if changed {
                            measured.set(Some(width));
                            cx.defer(move |cx| {
                                this.update(cx, |_, cx| cx.notify()).ok();
                            });
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }
}

impl Item for MailView {
    fn tab_title(&self, cx: &App) -> SharedString {
        self.thread_list.read(cx).title().clone()
    }

    fn tab_icon(&self, cx: &App) -> Option<IconName> {
        Some(match self.thread_list.read(cx).scope() {
            ThreadScope::AllFolders => IconName::Search,
            ThreadScope::Folder(_) => IconName::Inbox,
        })
    }

    fn serialize(&self, cx: &App) -> Option<SerializedItem> {
        let folder = SerializedFolder::of(self.thread_list.read(cx).scope());
        let data = serde_json::to_value(folder).ok()?;
        Some(SerializedItem::new(MAIL_KIND, data))
    }

    fn clone_on_split(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Entity<Self>> {
        let list = self.thread_list.read(cx);
        let (scope, title) = (list.scope().clone(), list.title().clone());
        let search = list.search().map(str::to_owned);
        let context = self.context.clone();
        Some(cx.new(|cx| Self::new(context, scope, title, search, window, cx)))
    }
}

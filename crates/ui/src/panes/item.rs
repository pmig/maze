//! What a tab shows: the [`Item`] trait (Zed's `workspace::Item`, much reduced), its
//! object-safe handle, and how items are written to and restored from the layout.

use std::fmt;
use std::rc::Rc;

use gpui::{
    AnyView, App, AppContext as _, Context, Entity, EntityId, EventEmitter, FocusHandle, Focusable,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use serde::{Deserialize, Serialize};

use crate::assets::IconName;
use crate::panes::Pane;

/// What an item tells its pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemEvent {
    /// The title, icon, tooltip or dirty state changed: redraw the tab.
    UpdateTab,
    /// The item is done (sent, discarded, or what it showed is gone): close its tab
    /// without asking.
    Close,
    /// What [`Item::serialize`] returns changed: the layout needs saving.
    Edited,
}

/// One tab as the layout file records it. `kind` picks the [`ItemKind`] that restores
/// it; `data` is the kind's own (ids only, never mail content).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SerializedItem {
    /// `"mail"`, `"message"`, `"compose"`, …
    pub kind: String,
    /// What the kind needs to restore the item.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
    /// A title that may be kept on disk (a draft's two-word name; never a subject), shown
    /// until the item is restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl SerializedItem {
    /// An item of `kind` with `data`.
    pub fn new(kind: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            data,
            title: None,
        }
    }

    /// With a title to show before the item is restored.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
}

/// A view that can live in a tab.
pub trait Item: Render + Focusable + EventEmitter<ItemEvent> {
    /// The tab label.
    fn tab_title(&self, cx: &App) -> SharedString;

    /// The icon before the label.
    fn tab_icon(&self, cx: &App) -> Option<IconName> {
        let _ = cx;
        None
    }

    /// The tab's tooltip.
    fn tab_tooltip(&self, cx: &App) -> Option<SharedString> {
        let _ = cx;
        None
    }

    /// Unsaved changes: the tab shows a dot instead of its close button.
    fn is_dirty(&self, cx: &App) -> bool {
        let _ = cx;
        false
    }

    /// The tab is about to close. Return `true` to close it now; `false` keeps it open
    /// (the item asks first, and emits [`ItemEvent::Close`] when it may go).
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let _ = (window, cx);
        true
    }

    /// What restores this item after a restart; `None` isn't kept.
    fn serialize(&self, cx: &App) -> Option<SerializedItem> {
        let _ = cx;
        None
    }

    /// A copy for the new pane of a split (`cmd-\`). `None` when the item can't be
    /// shown twice (a draft).
    fn clone_on_split(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Entity<Self>> {
        let _ = (window, cx);
        None
    }
}

/// An item of any type, as panes hold them.
pub trait ItemHandle: 'static {
    /// The item's entity id (unique among open items).
    fn item_id(&self) -> EntityId;
    /// See [`Item::tab_title`].
    fn tab_title(&self, cx: &App) -> SharedString;
    /// See [`Item::tab_icon`].
    fn tab_icon(&self, cx: &App) -> Option<IconName>;
    /// See [`Item::tab_tooltip`].
    fn tab_tooltip(&self, cx: &App) -> Option<SharedString>;
    /// See [`Item::is_dirty`].
    fn is_dirty(&self, cx: &App) -> bool;
    /// See [`Item::request_close`].
    fn request_close(&self, window: &mut Window, cx: &mut App) -> bool;
    /// See [`Item::serialize`].
    fn serialize(&self, cx: &App) -> Option<SerializedItem>;
    /// See [`Item::clone_on_split`].
    fn clone_on_split(&self, window: &mut Window, cx: &mut App) -> Option<Box<dyn ItemHandle>>;
    /// Where the focus goes when the tab is activated.
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    /// The view, to render or downcast.
    fn to_any_view(&self) -> AnyView;
    /// Another handle to the same item.
    fn boxed_clone(&self) -> Box<dyn ItemHandle>;
    /// Forwards the item's [`ItemEvent`]s to `pane`.
    fn subscribe_pane(&self, window: &mut Window, cx: &mut Context<'_, Pane>) -> Subscription;
}

impl<T: Item> ItemHandle for Entity<T> {
    fn item_id(&self) -> EntityId {
        self.entity_id()
    }

    fn tab_title(&self, cx: &App) -> SharedString {
        self.read(cx).tab_title(cx)
    }

    fn tab_icon(&self, cx: &App) -> Option<IconName> {
        self.read(cx).tab_icon(cx)
    }

    fn tab_tooltip(&self, cx: &App) -> Option<SharedString> {
        self.read(cx).tab_tooltip(cx)
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.read(cx).is_dirty(cx)
    }

    fn request_close(&self, window: &mut Window, cx: &mut App) -> bool {
        self.update(cx, |item, cx| item.request_close(window, cx))
    }

    fn serialize(&self, cx: &App) -> Option<SerializedItem> {
        self.read(cx).serialize(cx)
    }

    fn clone_on_split(&self, window: &mut Window, cx: &mut App) -> Option<Box<dyn ItemHandle>> {
        self.update(cx, |item, cx| item.clone_on_split(window, cx))
            .map(|clone| Box::new(clone) as Box<dyn ItemHandle>)
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.read(cx).focus_handle(cx)
    }

    fn to_any_view(&self) -> AnyView {
        self.clone().into()
    }

    fn boxed_clone(&self) -> Box<dyn ItemHandle> {
        Box::new(self.clone())
    }

    fn subscribe_pane(&self, window: &mut Window, cx: &mut Context<'_, Pane>) -> Subscription {
        let item_id = self.entity_id();
        cx.subscribe_in(
            self,
            window,
            move |pane, _, event: &ItemEvent, window, cx| {
                pane.on_item_event(item_id, *event, window, cx);
            },
        )
    }
}

impl dyn ItemHandle {
    /// The item as its concrete type.
    pub fn downcast<T: Item>(&self) -> Option<Entity<T>> {
        self.to_any_view().downcast::<T>().ok()
    }

    /// Whether the item is a `T`.
    pub fn is<T: Item>(&self) -> bool {
        self.downcast::<T>().is_some()
    }
}

impl Clone for Box<dyn ItemHandle> {
    fn clone(&self) -> Self {
        self.boxed_clone()
    }
}

impl fmt::Debug for dyn ItemHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemHandle")
            .field("id", &self.item_id())
            .finish_non_exhaustive()
    }
}

/// What restoring a [`SerializedItem`] gave.
pub enum Restore {
    /// The item.
    Item(Box<dyn ItemHandle>),
    /// Not possible yet (the mail data is still opening): keep the tab and try again
    /// later ([`PaneGroup::retry_pending`](super::PaneGroup::retry_pending)).
    Later,
    /// What it showed is gone (a deleted message or draft): drop the tab silently.
    Gone,
}

impl fmt::Debug for Restore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Item(item) => f.debug_tuple("Item").field(&item.item_id()).finish(),
            Self::Later => f.write_str("Later"),
            Self::Gone => f.write_str("Gone"),
        }
    }
}

/// Restores items of one kind.
pub type RestoreFn = Rc<dyn Fn(&SerializedItem, &mut Window, &mut App) -> Restore>;
/// The tab title of an item of one kind that isn't restored yet.
pub type TitleFn = Rc<dyn Fn(&SerializedItem, &App) -> Option<SharedString>>;

/// How items of one kind come back from the layout file. Registered with
/// [`PaneGroup::register_kind`](super::PaneGroup::register_kind).
#[derive(Clone)]
pub struct ItemKind {
    pub(crate) restore: RestoreFn,
    pub(crate) title: Option<TitleFn>,
    pub(crate) icon: Option<IconName>,
}

impl ItemKind {
    /// A kind restored by `restore`.
    pub fn new(
        restore: impl Fn(&SerializedItem, &mut Window, &mut App) -> Restore + 'static,
    ) -> Self {
        Self {
            restore: Rc::new(restore),
            title: None,
            icon: None,
        }
    }

    /// The title of a tab not restored yet (otherwise its stored title, or "…").
    pub fn title(
        mut self,
        title: impl Fn(&SerializedItem, &App) -> Option<SharedString> + 'static,
    ) -> Self {
        self.title = Some(Rc::new(title));
        self
    }

    /// The icon of a tab not restored yet.
    pub fn icon(mut self, icon: IconName) -> Self {
        self.icon = Some(icon);
        self
    }
}

impl fmt::Debug for ItemKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ItemKind").finish_non_exhaustive()
    }
}

/// A tab from the layout file that isn't restored yet: restoring waits until the tab
/// is shown (or, for drafts, until the mail data is open). It keeps what it was
/// restored from, so an unopened tab survives the next save.
pub struct PendingItem {
    serialized: SerializedItem,
    title: SharedString,
    icon: Option<IconName>,
    focus: FocusHandle,
}

impl fmt::Debug for PendingItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingItem")
            .field("kind", &self.serialized.kind)
            .finish_non_exhaustive()
    }
}

impl PendingItem {
    /// A placeholder for `serialized`.
    pub fn new(
        serialized: SerializedItem,
        title: SharedString,
        icon: Option<IconName>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| Self {
            serialized,
            title,
            icon,
            focus: cx.focus_handle(),
        })
    }

    /// What the tab will be restored from.
    pub fn serialized(&self) -> &SerializedItem {
        &self.serialized
    }

    /// Updates the title (once the kind that names it is registered).
    pub fn set_title(&mut self, title: SharedString, cx: &mut Context<'_, Self>) {
        if self.title != title {
            self.title = title;
            cx.emit(ItemEvent::UpdateTab);
        }
    }
}

impl EventEmitter<ItemEvent> for PendingItem {}

impl Focusable for PendingItem {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for PendingItem {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(theme.background)
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(div().child("Opening…"))
    }
}

impl Item for PendingItem {
    fn tab_title(&self, _: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_icon(&self, _: &App) -> Option<IconName> {
        self.icon
    }

    fn serialize(&self, _: &App) -> Option<SerializedItem> {
        Some(self.serialized.clone())
    }
}

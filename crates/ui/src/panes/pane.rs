//! A pane: a tab bar and the active tab's item below it (Zed's `Pane`, much reduced).
//!
//! Tabs are dragged with a [`DraggedTab`]: onto another tab (insert there), onto the
//! empty end of a tab bar (append), or onto a pane's content: its middle moves the tab
//! into that pane, its edges split the pane (the drop zone is highlighted). The pane
//! only reports drops ([`PaneEvent::Drop`]); its [`PaneGroup`](super::PaneGroup) moves
//! the tab, so every move goes through [`PaneGroup::move_item`](super::PaneGroup::move_item).

use std::fmt;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, DragMoveEvent, EntityId, EventEmitter,
    FocusHandle, Focusable, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    WeakEntity, Window, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    tooltip::Tooltip,
    v_flex,
};

use crate::assets::IconName;
use crate::panes::item::{ItemEvent, ItemHandle};
use crate::panes::tree::SplitDirection;
use crate::theme::scaled;

/// Height of a tab bar at the default font size ([`crate::theme::scaled`]).
pub const TAB_BAR_HEIGHT: f32 = 32.;
/// Tabs don't get wider than this (at the default font size); longer titles are cut.
const MAX_TAB_WIDTH: f32 = 220.;
/// Share of a pane's smaller side that counts as an edge for splitting drops.
const DROP_EDGE: f32 = 0.25;
/// Item activations remembered per pane (closing a tab goes back to the previous).
const HISTORY: usize = 32;

/// A tab being dragged; also its drag preview.
#[derive(Clone)]
pub struct DraggedTab {
    /// The pane the tab comes from.
    pub pane: WeakEntity<Pane>,
    /// The item.
    pub item_id: EntityId,
    title: SharedString,
    icon: Option<IconName>,
}

impl DraggedTab {
    /// A drag of `item_id` out of `pane`.
    pub fn new(pane: WeakEntity<Pane>, item_id: EntityId, title: SharedString) -> Self {
        Self {
            pane,
            item_id,
            title,
            icon: None,
        }
    }
}

impl fmt::Debug for DraggedTab {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DraggedTab")
            .field("item_id", &self.item_id)
            .finish_non_exhaustive()
    }
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        h_flex()
            .gap_1p5()
            .px_3()
            .h(scaled(TAB_BAR_HEIGHT - 4., cx))
            .max_w(scaled(MAX_TAB_WIDTH, cx))
            .rounded_md()
            .border_1()
            .border_color(theme.drag_border)
            .bg(theme.tab_active)
            .text_color(theme.tab_active_foreground)
            .text_sm()
            .shadow_md()
            .when_some(self.icon, |this, icon| this.child(Icon::new(icon).xsmall()))
            .child(div().truncate().child(self.title.clone()))
    }
}

/// Where a tab was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    /// Before the tab at this index (the tab count: at the end).
    Tab(usize),
    /// On the pane's content, away from its edges: into this pane, after its active tab.
    Center,
    /// On an edge of the pane's content: into a new pane on that side.
    Split(SplitDirection),
}

/// What a pane reports to its group.
#[derive(Debug, Clone)]
pub enum PaneEvent {
    /// The focus moved into the pane.
    Focused,
    /// Another tab became active (or the active tab's item was replaced).
    ActivatedItem,
    /// Tabs were added, removed or reordered.
    ItemsChanged,
    /// An item's saved state changed.
    ItemEdited,
    /// A tab was dropped on this pane.
    Drop {
        /// The dragged tab.
        tab: DraggedTab,
        /// Where.
        target: DropTarget,
    },
    /// The last tab closed.
    Empty,
}

/// What an empty pane shows.
pub type EmptyState = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// A tab bar and the active item.
pub struct Pane {
    items: Vec<Box<dyn ItemHandle>>,
    active: usize,
    history: Vec<EntityId>,
    focus: FocusHandle,
    is_active: bool,
    hovered_tab: Option<EntityId>,
    drop_zone: Option<DropTarget>,
    tab_scroll: ScrollHandle,
    empty_state: Option<EmptyState>,
    item_subscriptions: Vec<(EntityId, Subscription)>,
    _subscriptions: Vec<Subscription>,
}

impl fmt::Debug for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pane")
            .field("items", &self.items.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<PaneEvent> for Pane {}

impl Pane {
    /// An empty pane.
    pub fn new(
        empty_state: Option<EmptyState>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus_in(&focus, window, |_, _, cx| cx.emit(PaneEvent::Focused)),
            // Focus on the pane itself (a click on the tab bar's empty end) goes to
            // the active item.
            cx.on_focus(&focus, window, |this, window, cx| {
                this.focus_active_item(window, cx);
            }),
        ];
        Self {
            items: Vec::new(),
            active: 0,
            history: Vec::new(),
            focus,
            is_active: false,
            hovered_tab: None,
            drop_zone: None,
            tab_scroll: ScrollHandle::new(),
            empty_state,
            item_subscriptions: Vec::new(),
            _subscriptions: subscriptions,
        }
    }

    // ---- reading ----------------------------------------------------------------------

    /// The tabs' items, in order.
    pub fn items(&self) -> &[Box<dyn ItemHandle>] {
        &self.items
    }

    /// Number of tabs.
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    /// The active tab's index.
    pub fn active_index(&self) -> usize {
        self.active
    }

    /// The active tab's item.
    pub fn active_item(&self) -> Option<&dyn ItemHandle> {
        self.items.get(self.active).map(|item| item.as_ref())
    }

    /// The index of the tab showing `item_id`.
    pub fn index_of(&self, item_id: EntityId) -> Option<usize> {
        self.items.iter().position(|item| item.item_id() == item_id)
    }

    /// Whether this is the group's active pane.
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    pub(crate) fn set_active(&mut self, active: bool, cx: &mut Context<'_, Self>) {
        if self.is_active != active {
            self.is_active = active;
            cx.notify();
        }
    }

    // ---- changing ---------------------------------------------------------------------

    /// Adds a tab at `index` (default: after the active tab). An item that has a tab
    /// here already is activated instead.
    pub fn add_item(
        &mut self,
        item: Box<dyn ItemHandle>,
        index: Option<usize>,
        activate: bool,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(ix) = self.index_of(item.item_id()) {
            if activate {
                self.activate(ix, focus, window, cx);
            }
            return;
        }
        let ix = index
            .unwrap_or(if self.items.is_empty() {
                0
            } else {
                self.active + 1
            })
            .min(self.items.len());
        let subscription = item.subscribe_pane(window, cx);
        self.item_subscriptions.push((item.item_id(), subscription));
        self.items.insert(ix, item);
        if ix <= self.active && self.items.len() > 1 {
            self.active += 1;
        }
        cx.emit(PaneEvent::ItemsChanged);
        if activate || self.items.len() == 1 {
            self.activate(ix, focus, window, cx);
        }
        cx.notify();
    }

    /// Activates tab `ix`, focusing its item if `focus`.
    pub fn activate(
        &mut self,
        ix: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(item) = self.items.get(ix) else {
            return;
        };
        let item_id = item.item_id();
        let changed = ix != self.active || self.history.last() != Some(&item_id);
        self.active = ix;
        self.history.retain(|id| *id != item_id);
        self.history.push(item_id);
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
        self.tab_scroll.scroll_to_item(ix);
        if focus {
            self.focus_active_item(window, cx);
        }
        if changed {
            cx.emit(PaneEvent::ActivatedItem);
        }
        cx.notify();
    }

    /// Activates the next tab, wrapping around.
    pub fn activate_next(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.items.is_empty() {
            let ix = (self.active + 1) % self.items.len();
            self.activate(ix, true, window, cx);
        }
    }

    /// Activates the previous tab, wrapping around.
    pub fn activate_previous(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.items.is_empty() {
            let ix = (self.active + self.items.len() - 1) % self.items.len();
            self.activate(ix, true, window, cx);
        }
    }

    /// Focuses the active item (or the pane, when it is empty).
    pub fn focus_active_item(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        match self.items.get(self.active) {
            Some(item) => {
                let handle = item.focus_handle(cx);
                if !handle.contains_focused(window, cx) {
                    window.focus(&handle, cx);
                }
            }
            None => {
                if !self.focus.is_focused(window) {
                    window.focus(&self.focus, cx);
                }
            }
        }
    }

    /// Asks the item to close: at once, or after it asked the user (it then emits
    /// [`ItemEvent::Close`]).
    pub fn close_item(
        &mut self,
        item_id: EntityId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(item) = self
            .index_of(item_id)
            .map(|ix| self.items[ix].boxed_clone())
        else {
            return;
        };
        if item.request_close(window, cx) {
            self.remove_item(item_id, window, cx);
        }
    }

    /// Closes the active tab (asking first where the item does).
    pub fn close_active_item(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(item_id) = self.active_item().map(|item| item.item_id()) {
            self.close_item(item_id, window, cx);
        }
    }

    /// Takes a tab out without asking (it moves to another pane, or its item is done).
    pub fn take_item(
        &mut self,
        item_id: EntityId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Box<dyn ItemHandle>> {
        let ix = self.index_of(item_id)?;
        let had_focus = self.focus.contains_focused(window, cx);
        let item = self.items.remove(ix);
        self.item_subscriptions.retain(|(id, _)| *id != item_id);
        self.history.retain(|id| *id != item_id);
        if self.items.is_empty() {
            self.active = 0;
            cx.emit(PaneEvent::ItemsChanged);
            cx.emit(PaneEvent::Empty);
            cx.notify();
            return Some(item);
        }
        let was_active = ix == self.active;
        if ix < self.active {
            self.active -= 1;
        }
        cx.emit(PaneEvent::ItemsChanged);
        if was_active {
            // Back to the tab used before, else the neighbor.
            let next = self
                .history
                .last()
                .and_then(|id| self.index_of(*id))
                .unwrap_or(ix.min(self.items.len() - 1));
            self.activate(next, had_focus, window, cx);
        }
        cx.notify();
        Some(item)
    }

    /// Removes a tab (see [`Pane::take_item`]).
    pub fn remove_item(
        &mut self,
        item_id: EntityId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        drop(self.take_item(item_id, window, cx));
    }

    /// Puts `item` in place of tab `ix` (a restored placeholder).
    pub fn replace_item(
        &mut self,
        ix: usize,
        item: Box<dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(old_id) = self.items.get(ix).map(|item| item.item_id()) else {
            return;
        };
        let had_focus = ix == self.active && self.focus.contains_focused(window, cx);
        self.item_subscriptions.retain(|(id, _)| *id != old_id);
        let subscription = item.subscribe_pane(window, cx);
        let new_id = item.item_id();
        self.item_subscriptions.push((new_id, subscription));
        self.items[ix] = item;
        for id in &mut self.history {
            if *id == old_id {
                *id = new_id;
            }
        }
        if ix == self.active {
            if had_focus {
                self.focus_active_item(window, cx);
            }
            cx.emit(PaneEvent::ActivatedItem);
        }
        cx.emit(PaneEvent::ItemEdited);
        cx.notify();
    }

    /// Moves a tab within this pane to `to` (an index in the current order).
    pub fn move_item_to(&mut self, item_id: EntityId, to: usize, cx: &mut Context<'_, Self>) {
        let Some(from) = self.index_of(item_id) else {
            return;
        };
        let active_id = self.active_item().map(|item| item.item_id());
        let item = self.items.remove(from);
        // Dropping before a later tab: that tab moved one to the left.
        let to = if to > from { to - 1 } else { to }.min(self.items.len());
        self.items.insert(to, item);
        if let Some(active) = active_id.and_then(|id| self.index_of(id)) {
            self.active = active;
        }
        if from != to {
            cx.emit(PaneEvent::ItemsChanged);
        }
        cx.notify();
    }

    pub(crate) fn on_item_event(
        &mut self,
        item_id: EntityId,
        event: ItemEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            ItemEvent::UpdateTab => cx.notify(),
            ItemEvent::Close => self.remove_item(item_id, window, cx),
            ItemEvent::Edited => cx.emit(PaneEvent::ItemEdited),
        }
    }

    // ---- drag and drop ----------------------------------------------------------------

    fn on_content_drag_move(
        &mut self,
        event: &DragMoveEvent<DraggedTab>,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let bounds = event.bounds;
        let position = event.event.position;
        let zone = bounds.contains(&position).then(|| {
            let edge = bounds.size.width.min(bounds.size.height) * DROP_EDGE;
            let left = position.x - bounds.left();
            let right = bounds.right() - position.x;
            let top = position.y - bounds.top();
            let bottom = bounds.bottom() - position.y;
            let nearest = [
                (SplitDirection::Up, top),
                (SplitDirection::Right, right),
                (SplitDirection::Down, bottom),
                (SplitDirection::Left, left),
            ]
            .into_iter()
            .min_by(|a, b| f32::from(a.1).total_cmp(&f32::from(b.1)));
            match nearest {
                Some((direction, distance)) if distance < edge => DropTarget::Split(direction),
                _ => DropTarget::Center,
            }
        });
        if zone != self.drop_zone {
            self.drop_zone = zone;
            cx.notify();
        }
    }

    fn drop_tab(&mut self, tab: &DraggedTab, target: DropTarget, cx: &mut Context<'_, Self>) {
        self.drop_zone = None;
        cx.emit(PaneEvent::Drop {
            tab: tab.clone(),
            target,
        });
        cx.notify();
    }

    // ---- rendering --------------------------------------------------------------------

    fn render_tab(
        &self,
        ix: usize,
        item: &dyn ItemHandle,
        cx: &Context<'_, Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().colors;
        let item_id = item.item_id();
        let active = ix == self.active;
        let hovered = self.hovered_tab == Some(item_id);
        let dirty = item.is_dirty(cx);
        let title = item.tab_title(cx);
        let icon = item.tab_icon(cx);
        let tooltip = item.tab_tooltip(cx);
        let drag = DraggedTab {
            pane: cx.weak_entity(),
            item_id,
            title: title.clone(),
            icon,
        };
        let close = Button::new(("close-tab", item_id.as_u64()))
            .ghost()
            .xsmall()
            .icon(Icon::new(IconName::Close))
            .tooltip("Close Tab (⌘W)")
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.close_item(item_id, window, cx);
            }));
        let indicator = if dirty && !hovered {
            div()
                .debug_selector(move || format!("tab-dirty-{ix}"))
                .size(scaled(18., cx))
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(7.)).rounded_full().bg(theme.foreground))
                .into_any_element()
        } else if hovered || active {
            div().child(close).into_any_element()
        } else {
            div().size(scaled(18., cx)).into_any_element()
        };
        h_flex()
            .id(("pane-tab", item_id.as_u64()))
            .debug_selector(move || format!("pane-tab-{ix}"))
            .relative()
            .h_full()
            .flex_none()
            .max_w(scaled(MAX_TAB_WIDTH, cx))
            .pl_3()
            .pr_1()
            .gap_1p5()
            .border_r_1()
            .border_color(theme.border)
            .text_sm()
            .cursor_pointer()
            .map(|this| {
                if active {
                    this.bg(theme.tab_active)
                        .text_color(theme.tab_active_foreground)
                } else {
                    this.bg(theme.tab).text_color(theme.tab_foreground)
                }
            })
            .when(active && self.is_active, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(2.))
                        .bg(theme.primary),
                )
            })
            .when_some(icon, |this, icon| {
                this.child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
            })
            .child(div().min_w_0().truncate().child(title))
            .child(indicator)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let next = if *hovered {
                    Some(item_id)
                } else if this.hovered_tab == Some(item_id) {
                    None
                } else {
                    this.hovered_tab
                };
                if next != this.hovered_tab {
                    this.hovered_tab = next;
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if let Some(ix) = this.index_of(item_id) {
                        this.activate(ix, true, window, cx);
                    }
                }),
            )
            .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.is_middle_click() {
                    this.close_item(item_id, window, cx);
                }
            }))
            .on_drag(drag, |tab, _, _, cx| cx.new(|_| tab.clone()))
            .drag_over::<DraggedTab>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(move |this, tab: &DraggedTab, _, cx| {
                let ix = this.index_of(item_id).unwrap_or(ix);
                this.drop_tab(tab, DropTarget::Tab(ix), cx);
            }))
            .when_some(tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            })
    }

    fn render_tab_bar(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let count = self.items.len();
        let tabs: Vec<AnyElement> = self
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| self.render_tab(ix, item.as_ref(), cx).into_any_element())
            .collect();
        h_flex()
            .id("pane-tab-bar")
            .h(scaled(TAB_BAR_HEIGHT, cx))
            .w_full()
            .flex_none()
            .bg(theme.tab_bar)
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .id("pane-tabs")
                    .h_full()
                    .min_w_0()
                    .flex_shrink(1.)
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(tabs),
            )
            .child(
                div()
                    .id("pane-tab-bar-end")
                    .h_full()
                    .flex_1()
                    .min_w(scaled(24., cx))
                    .drag_over::<DraggedTab>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                    .on_drop(cx.listener(move |this, tab: &DraggedTab, _, cx| {
                        this.drop_tab(tab, DropTarget::Tab(count), cx);
                    }))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.focus_active_item(window, cx);
                    })),
            )
    }
}

impl Focusable for Pane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Pane {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let content: AnyElement = match self.items.get(self.active) {
            Some(item) => item.to_any_view().into_any_element(),
            None => match self.empty_state.clone() {
                Some(empty) => empty(window, cx),
                None => v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No open tabs")
                    .into_any_element(),
            },
        };
        let zone = self.drop_zone.filter(|_| cx.has_active_drag());
        v_flex()
            .size_full()
            .key_context("Pane")
            .track_focus(&self.focus)
            .bg(theme.background)
            .child(self.render_tab_bar(cx))
            .child(
                div()
                    .id("pane-content")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(content)
                    .on_drag_move(cx.listener(Self::on_content_drag_move))
                    .on_drop(cx.listener(|this, tab: &DraggedTab, _, cx| {
                        let target = match this.drop_zone {
                            Some(DropTarget::Split(direction)) => DropTarget::Split(direction),
                            _ => DropTarget::Center,
                        };
                        this.drop_tab(tab, target, cx);
                    }))
                    .when_some(zone, |this, zone| {
                        let half = relative(0.5);
                        this.child(
                            div()
                                .absolute()
                                .bg(theme.drop_target)
                                .map(|this| match zone {
                                    DropTarget::Split(SplitDirection::Up) => {
                                        this.top_0().left_0().right_0().h(half)
                                    }
                                    DropTarget::Split(SplitDirection::Down) => {
                                        this.bottom_0().left_0().right_0().h(half)
                                    }
                                    DropTarget::Split(SplitDirection::Left) => {
                                        this.top_0().bottom_0().left_0().w(half)
                                    }
                                    DropTarget::Split(SplitDirection::Right) => {
                                        this.top_0().bottom_0().right_0().w(half)
                                    }
                                    DropTarget::Center | DropTarget::Tab(_) => {
                                        this.top_0().bottom_0().left_0().right_0()
                                    }
                                }),
                        )
                    }),
            )
    }
}

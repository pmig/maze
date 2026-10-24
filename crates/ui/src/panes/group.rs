//! The pane group: panes in splits, the active pane, tab moves, the layout file.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Axis, Bounds, Context, DragMoveEvent, Empty, Entity,
    EntityId, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _, Pixels,
    Render, SharedString, Styled as _, Subscription, Window, canvas, div,
    prelude::FluentBuilder as _, relative,
};
use gpui_kit::component::{
    ActiveTheme as _, h_flex,
    resizable::{HandleEdge, resize_handle},
    v_flex,
};

use crate::panes::item::{Item, ItemHandle, ItemKind, PendingItem, Restore, SerializedItem};
use crate::panes::pane::{DropTarget, EmptyState, Pane, PaneEvent};
use crate::panes::tree::{Member, PaneTree, Split, SplitDirection, SplitId};
use crate::panes::{SerializedLayout, SerializedMember, SerializedPane};

/// Item activations remembered across panes (for "the most recent mail view").
const HISTORY: usize = 64;

/// What the group reports to the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneGroupEvent {
    /// Splits, sizes, tabs or what they show changed: save the layout.
    LayoutChanged,
    /// The active pane or its active tab changed.
    ActiveItemChanged,
}

/// Gives the new pane of a keyboard split its item when the active one can't be
/// shown twice.
pub type SplitFallback = Rc<dyn Fn(&mut Window, &mut App) -> Option<Box<dyn ItemHandle>>>;

/// A split being resized; also the drag payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SplitDrag {
    split: SplitId,
    ix: usize,
}

impl Render for SplitDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        Empty
    }
}

/// Where panes and splits were drawn last (focus by direction, split resizing).
#[derive(Debug, Default)]
struct Drawn {
    panes: HashMap<EntityId, Bounds<Pixels>>,
    splits: HashMap<SplitId, Bounds<Pixels>>,
}

/// Panes in splits.
pub struct PaneGroup {
    tree: PaneTree<Entity<Pane>>,
    active_pane: Entity<Pane>,
    kinds: HashMap<String, ItemKind>,
    drawn: Rc<RefCell<Drawn>>,
    history: Vec<EntityId>,
    split_fallback: Option<SplitFallback>,
    empty_state: Option<EmptyState>,
    pane_subscriptions: Vec<(EntityId, Subscription)>,
}

impl std::fmt::Debug for PaneGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaneGroup")
            .field("panes", &self.tree.panes().len())
            .finish_non_exhaustive()
    }
}

impl EventEmitter<PaneGroupEvent> for PaneGroup {}

fn new_pane(
    empty_state: Option<EmptyState>,
    window: &mut Window,
    cx: &mut Context<'_, PaneGroup>,
) -> (Entity<Pane>, (EntityId, Subscription)) {
    let pane = cx.new(|cx| Pane::new(empty_state, window, cx));
    let subscription = cx.subscribe_in(&pane, window, PaneGroup::on_pane_event);
    let id = pane.entity_id();
    (pane, (id, subscription))
}

impl PaneGroup {
    /// One empty pane. `empty_state` draws a pane without tabs.
    pub fn new(
        empty_state: Option<EmptyState>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let (pane, subscription) = new_pane(empty_state.clone(), window, cx);
        pane.update(cx, |pane, cx| pane.set_active(true, cx));
        Self {
            tree: PaneTree::new(pane.clone()),
            active_pane: pane,
            kinds: HashMap::new(),
            drawn: Rc::default(),
            history: Vec::new(),
            split_fallback: None,
            empty_state,
            pane_subscriptions: vec![subscription],
        }
    }

    /// What a keyboard split puts in the new pane when the active item can't be cloned.
    pub fn set_split_fallback(&mut self, fallback: SplitFallback) {
        self.split_fallback = Some(fallback);
    }

    // ---- reading ----------------------------------------------------------------------

    /// The pane that gets new tabs: the one the focus was in last.
    pub fn active_pane(&self) -> &Entity<Pane> {
        &self.active_pane
    }

    /// Every pane, left to right and top to bottom.
    pub fn panes(&self) -> Vec<Entity<Pane>> {
        self.tree.panes().into_iter().cloned().collect()
    }

    /// The layout tree.
    pub fn tree(&self) -> &PaneTree<Entity<Pane>> {
        &self.tree
    }

    /// The active pane's active item.
    pub fn active_item(&self, cx: &App) -> Option<Box<dyn ItemHandle>> {
        self.active_pane
            .read(cx)
            .active_item()
            .map(|item| item.boxed_clone())
    }

    /// Every open item.
    pub fn items(&self, cx: &App) -> Vec<Box<dyn ItemHandle>> {
        self.tree
            .panes()
            .into_iter()
            .flat_map(|pane| pane.read(cx).items().iter().map(|item| item.boxed_clone()))
            .collect()
    }

    /// Every open item of type `T`.
    pub fn items_of<T: Item>(&self, cx: &App) -> Vec<Entity<T>> {
        self.items(cx)
            .iter()
            .filter_map(|item| item.downcast::<T>())
            .collect()
    }

    /// The `T` activated last (or any `T` when none was activated yet).
    pub fn most_recent<T: Item>(&self, cx: &App) -> Option<Entity<T>> {
        let items = self.items_of::<T>(cx);
        self.history
            .iter()
            .rev()
            .find_map(|id| items.iter().find(|item| item.entity_id() == *id).cloned())
            .or_else(|| items.into_iter().next())
    }

    /// The pane showing `item_id`.
    pub fn pane_of(&self, item_id: EntityId, cx: &App) -> Option<Entity<Pane>> {
        self.tree
            .panes()
            .into_iter()
            .find(|pane| pane.read(cx).index_of(item_id).is_some())
            .cloned()
    }

    // ---- items ------------------------------------------------------------------------

    /// Opens `item` in the active pane, after its active tab.
    pub fn add_item(
        &mut self,
        item: Box<dyn ItemHandle>,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| {
            pane.add_item(item, None, true, focus, window, cx)
        });
    }

    /// Activates the tab of `item_id` (in whichever pane), focusing it if `focus`.
    pub fn activate_item(
        &mut self,
        item_id: EntityId,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(pane) = self.pane_of(item_id, cx) else {
            return false;
        };
        pane.update(cx, |pane, cx| {
            if let Some(ix) = pane.index_of(item_id) {
                pane.activate(ix, focus, window, cx);
            }
        });
        self.set_active_pane(pane, cx);
        true
    }

    /// Closes the active tab (asking first where the item does).
    pub fn close_active_item(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| pane.close_active_item(window, cx));
    }

    /// Asks `item_id` to close.
    pub fn close_item(
        &mut self,
        item_id: EntityId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if let Some(pane) = self.pane_of(item_id, cx) {
            pane.update(cx, |pane, cx| pane.close_item(item_id, window, cx));
        }
    }

    /// The next tab of the active pane.
    pub fn activate_next_item(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| pane.activate_next(window, cx));
    }

    /// The previous tab of the active pane.
    pub fn activate_previous_item(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| pane.activate_previous(window, cx));
    }

    /// Tab `ix` of the active pane.
    pub fn activate_item_at(&mut self, ix: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| pane.activate(ix, true, window, cx));
    }

    /// The last tab of the active pane.
    pub fn activate_last_item(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pane = self.active_pane.clone();
        pane.update(cx, |pane, cx| {
            if let Some(last) = pane.item_count().checked_sub(1) {
                pane.activate(last, true, window, cx);
            }
        });
    }

    // ---- panes ------------------------------------------------------------------------

    fn set_active_pane(&mut self, pane: Entity<Pane>, cx: &mut Context<'_, Self>) {
        if pane == self.active_pane || !self.tree.contains(&pane) {
            return;
        }
        self.active_pane
            .update(cx, |pane, cx| pane.set_active(false, cx));
        pane.update(cx, |pane, cx| pane.set_active(true, cx));
        self.active_pane = pane;
        cx.emit(PaneGroupEvent::ActiveItemChanged);
        cx.emit(PaneGroupEvent::LayoutChanged);
        cx.notify();
    }

    /// Adds an empty pane next to `pane` and makes it active.
    pub fn split_pane(
        &mut self,
        pane: &Entity<Pane>,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<Pane> {
        let (new, subscription) = new_pane(self.empty_state.clone(), window, cx);
        self.pane_subscriptions.push(subscription);
        if !self.tree.split(pane, new.clone(), direction) {
            let root = self.tree.panes().first().map(|p| (*p).clone());
            if let Some(root) = root {
                self.tree.split(&root, new.clone(), direction);
            }
        }
        self.set_active_pane(new.clone(), cx);
        cx.emit(PaneGroupEvent::LayoutChanged);
        cx.notify();
        new
    }

    /// Splits the active pane (`cmd-\`): the new pane shows a copy of the active item,
    /// or what the split fallback gives for an item that can't be shown twice.
    pub fn split(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let source = self.active_pane.clone();
        let active = source.read(cx).active_item().map(|item| item.boxed_clone());
        let item = active
            .and_then(|item| item.clone_on_split(window, cx))
            .or_else(|| {
                self.split_fallback
                    .clone()
                    .and_then(|fallback| fallback(window, cx))
            });
        let pane = self.split_pane(&source, direction, window, cx);
        pane.update(cx, |pane, cx| match item {
            Some(item) => pane.add_item(item, None, true, true, window, cx),
            None => pane.focus_active_item(window, cx),
        });
    }

    /// Moves the tab of `item_id` from `from` to `target` on `to`: what a tab drop
    /// does. A pane left without tabs closes (unless it is the last).
    pub fn move_item(
        &mut self,
        from: &Entity<Pane>,
        item_id: EntityId,
        to: &Entity<Pane>,
        target: DropTarget,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if from.read(cx).index_of(item_id).is_none() || !self.tree.contains(to) {
            return false;
        }
        match target {
            DropTarget::Split(direction) => {
                // A pane's only tab dropped on its own edge: nothing would change.
                if from == to && from.read(cx).item_count() == 1 {
                    return false;
                }
                let pane = self.split_pane(to, direction, window, cx);
                let Some(item) = from.update(cx, |pane, cx| pane.take_item(item_id, window, cx))
                else {
                    return false;
                };
                pane.update(cx, |pane, cx| {
                    pane.add_item(item, None, true, true, window, cx)
                });
            }
            DropTarget::Tab(ix) if from == to => {
                to.update(cx, |pane, cx| {
                    pane.move_item_to(item_id, ix, cx);
                    if let Some(ix) = pane.index_of(item_id) {
                        pane.activate(ix, true, window, cx);
                    }
                });
            }
            DropTarget::Center if from == to => {
                to.update(cx, |pane, cx| {
                    if let Some(ix) = pane.index_of(item_id) {
                        pane.activate(ix, true, window, cx);
                    }
                });
            }
            DropTarget::Tab(_) | DropTarget::Center => {
                let index = match target {
                    DropTarget::Tab(ix) => Some(ix),
                    _ => None,
                };
                let Some(item) = from.update(cx, |pane, cx| pane.take_item(item_id, window, cx))
                else {
                    return false;
                };
                to.update(cx, |pane, cx| {
                    pane.add_item(item, index, true, true, window, cx)
                });
                self.set_active_pane(to.clone(), cx);
            }
        }
        cx.emit(PaneGroupEvent::LayoutChanged);
        cx.notify();
        true
    }

    /// Focuses the pane in `direction` of the active one (by where they were drawn).
    pub fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let target = {
            let drawn = self.drawn.borrow();
            self.tree
                .neighbor(&self.active_pane, direction, |pane| {
                    drawn.panes.get(&pane.entity_id()).copied()
                })
                .cloned()
        };
        if let Some(pane) = target {
            pane.update(cx, |pane, cx| pane.focus_active_item(window, cx));
            self.set_active_pane(pane, cx);
        }
    }

    fn remove_pane_if_empty(
        &mut self,
        pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if pane.read(cx).item_count() > 0 || !self.tree.remove(pane) {
            return;
        }
        let id = pane.entity_id();
        self.pane_subscriptions.retain(|(pane, _)| *pane != id);
        self.drawn.borrow_mut().panes.remove(&id);
        if &self.active_pane == pane {
            let next = self.tree.panes().first().map(|p| (*p).clone());
            if let Some(next) = next {
                next.update(cx, |pane, cx| {
                    pane.set_active(true, cx);
                    pane.focus_active_item(window, cx);
                });
                self.active_pane = next;
                cx.emit(PaneGroupEvent::ActiveItemChanged);
            }
        }
        cx.emit(PaneGroupEvent::LayoutChanged);
        cx.notify();
    }

    fn on_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            PaneEvent::Focused => self.set_active_pane(pane.clone(), cx),
            PaneEvent::ActivatedItem => {
                if let Some(id) = pane.read(cx).active_item().map(|item| item.item_id()) {
                    self.history.retain(|other| *other != id);
                    self.history.push(id);
                    if self.history.len() > HISTORY {
                        self.history.remove(0);
                    }
                }
                self.restore_active(pane, window, cx);
                if pane == &self.active_pane {
                    cx.emit(PaneGroupEvent::ActiveItemChanged);
                }
                cx.emit(PaneGroupEvent::LayoutChanged);
            }
            PaneEvent::ItemsChanged => {
                if pane == &self.active_pane {
                    cx.emit(PaneGroupEvent::ActiveItemChanged);
                }
                cx.emit(PaneGroupEvent::LayoutChanged);
            }
            PaneEvent::ItemEdited => cx.emit(PaneGroupEvent::LayoutChanged),
            PaneEvent::Drop { tab, target } => {
                if let Some(from) = tab.pane.upgrade() {
                    self.move_item(&from, tab.item_id, pane, *target, window, cx);
                }
            }
            PaneEvent::Empty => self.remove_pane_if_empty(pane, window, cx),
        }
        cx.notify();
    }

    // ---- the layout file --------------------------------------------------------------

    /// Registers how items of `kind` are restored, and restores the shown tabs of that
    /// kind that were waiting for it.
    pub fn register_kind(
        &mut self,
        kind: impl Into<String>,
        item_kind: ItemKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.kinds.insert(kind.into(), item_kind);
        self.retry_pending(window, cx);
    }

    /// Restores the shown tabs that couldn't be restored before (the mail data is
    /// open now), and renames the others (their kinds may know their titles now).
    pub fn retry_pending(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        for pending in self.items_of::<PendingItem>(cx) {
            let title = {
                let serialized = pending.read(cx).serialized();
                self.kinds
                    .get(&serialized.kind)
                    .and_then(|kind| kind.title.as_ref())
                    .and_then(|title| title(serialized, cx))
            };
            if let Some(title) = title {
                pending.update(cx, |pending, cx| pending.set_title(title, cx));
            }
        }
        for pane in self.panes() {
            self.restore_active(&pane, window, cx);
        }
    }

    fn restore_active(
        &mut self,
        pane: &Entity<Pane>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let (ix, item_id, serialized) = {
            let pane = pane.read(cx);
            let Some(pending) = pane
                .active_item()
                .and_then(|item| item.downcast::<PendingItem>())
            else {
                return;
            };
            (
                pane.active_index(),
                pending.entity_id(),
                pending.read(cx).serialized().clone(),
            )
        };
        let Some(kind) = self.kinds.get(&serialized.kind).cloned() else {
            return;
        };
        match (kind.restore)(&serialized, window, cx) {
            Restore::Item(item) => {
                pane.update(cx, |pane, cx| pane.replace_item(ix, item, window, cx));
            }
            Restore::Gone => {
                pane.update(cx, |pane, cx| pane.remove_item(item_id, window, cx));
            }
            Restore::Later => {}
        }
    }

    fn pending_item(&self, serialized: SerializedItem, cx: &mut App) -> Box<dyn ItemHandle> {
        let kind = self.kinds.get(&serialized.kind);
        let title: SharedString = kind
            .and_then(|kind| kind.title.as_ref())
            .and_then(|title| title(&serialized, cx))
            .or_else(|| serialized.title.clone().map(SharedString::from))
            .unwrap_or_else(|| "…".into());
        let icon = kind.and_then(|kind| kind.icon);
        Box::new(PendingItem::new(serialized, title, icon, cx))
    }

    /// The layout as `ui_state.json` keeps it. Tabs that can't be restored are left
    /// out, and so are panes left without tabs.
    pub fn serialize(&self, cx: &App) -> SerializedLayout {
        fn walk(
            member: &Member<Entity<Pane>>,
            active: &Entity<Pane>,
            active_ix: &mut Option<usize>,
            count: &mut usize,
            cx: &App,
        ) -> Option<SerializedMember> {
            match member {
                Member::Pane(pane) => {
                    let pane_ref = pane.read(cx);
                    let mut items = Vec::new();
                    let mut active_item = 0;
                    for (ix, item) in pane_ref.items().iter().enumerate() {
                        if let Some(serialized) = item.serialize(cx) {
                            if ix <= pane_ref.active_index() {
                                active_item = items.len();
                            }
                            items.push(serialized);
                        }
                    }
                    if items.is_empty() {
                        return None;
                    }
                    if pane == active {
                        *active_ix = Some(*count);
                    }
                    *count += 1;
                    Some(SerializedMember::Pane(SerializedPane {
                        active: active_item,
                        items,
                    }))
                }
                Member::Split(split) => {
                    let (members, flexes): (Vec<_>, Vec<_>) = split
                        .members
                        .iter()
                        .zip(&split.flexes)
                        .filter_map(|(m, f)| Some((walk(m, active, active_ix, count, cx)?, *f)))
                        .unzip();
                    match members.len() {
                        0 => None,
                        1 => members.into_iter().next(),
                        _ => Some(SerializedMember::Split {
                            axis: split.axis.into(),
                            members,
                            flexes,
                        }),
                    }
                }
            }
        }
        let mut active_pane = None;
        let mut count = 0;
        let root = walk(
            self.tree.root(),
            &self.active_pane,
            &mut active_pane,
            &mut count,
            cx,
        )
        .unwrap_or(SerializedMember::Pane(SerializedPane::default()));
        SerializedLayout {
            root,
            active_pane: active_pane.unwrap_or(0),
        }
    }

    /// Replaces the panes with `layout`. Each pane's active tab is restored now, the
    /// others when they are first shown. Returns `false` (and changes nothing) when
    /// the layout has no tabs.
    pub fn load(
        &mut self,
        layout: SerializedLayout,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        fn convert(member: SerializedMember) -> Option<Member<SerializedPane>> {
            match member {
                SerializedMember::Pane(pane) if pane.items.is_empty() => None,
                SerializedMember::Pane(pane) => Some(Member::Pane(pane)),
                SerializedMember::Split {
                    axis,
                    members,
                    flexes,
                } => {
                    let (members, flexes): (Vec<_>, Vec<_>) = members
                        .into_iter()
                        .zip(flexes.into_iter().chain(std::iter::repeat(f32::NAN)))
                        .filter_map(|(m, f)| Some((convert(m)?, f)))
                        .unzip();
                    Some(Member::Split(Split {
                        id: SplitId(0),
                        axis: axis.into(),
                        members,
                        flexes,
                    }))
                }
            }
        }
        // `PaneTree::from_member` needs `PartialEq` panes; index the panes instead.
        let mut panes: Vec<SerializedPane> = Vec::new();
        let Some(indexed) = convert(layout.root).map(|member| index_panes(member, &mut panes))
        else {
            return false;
        };
        let Some(shape) = PaneTree::from_member(indexed) else {
            return false;
        };

        let mut subscriptions = Vec::new();
        let mut created: Vec<Entity<Pane>> = Vec::new();
        for pane_state in &panes {
            let (pane, subscription) = new_pane(self.empty_state.clone(), window, cx);
            subscriptions.push(subscription);
            let items: Vec<_> = pane_state
                .items
                .iter()
                .map(|item| self.pending_item(item.clone(), cx))
                .collect();
            let active = pane_state.active.min(items.len().saturating_sub(1));
            pane.update(cx, |pane, cx| {
                for item in items {
                    let at = pane.item_count();
                    pane.add_item(item, Some(at), false, false, window, cx);
                }
                pane.activate(active, false, window, cx);
            });
            created.push(pane);
        }
        let member = shape.map(|ix| created[*ix].clone());
        let Some(tree) = PaneTree::from_member(member) else {
            return false;
        };
        let order: Vec<Entity<Pane>> = tree.panes().into_iter().cloned().collect();
        let active = order
            .get(layout.active_pane)
            .or_else(|| order.first())
            .cloned()
            .unwrap_or_else(|| self.active_pane.clone());

        self.tree = tree;
        self.pane_subscriptions = subscriptions;
        self.drawn.borrow_mut().panes.clear();
        self.drawn.borrow_mut().splits.clear();
        for pane in &order {
            pane.update(cx, |p, cx| p.set_active(*pane == active, cx));
        }
        self.active_pane = active;
        self.retry_pending(window, cx);
        cx.emit(PaneGroupEvent::LayoutChanged);
        cx.emit(PaneGroupEvent::ActiveItemChanged);
        cx.notify();
        true
    }

    // ---- rendering --------------------------------------------------------------------

    fn on_split_drag(
        &mut self,
        event: &DragMoveEvent<SplitDrag>,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let drag = *event.drag(cx);
        let Some(bounds) = self.drawn.borrow().splits.get(&drag.split).copied() else {
            return;
        };
        let Some(axis) = self.tree.split_mut(drag.split).map(|split| split.axis) else {
            return;
        };
        let position = event.event.position;
        let fraction = match axis {
            Axis::Horizontal => {
                f32::from(position.x - bounds.left()) / f32::from(bounds.size.width).max(1.)
            }
            Axis::Vertical => {
                f32::from(position.y - bounds.top()) / f32::from(bounds.size.height).max(1.)
            }
        };
        if self.tree.resize(drag.split, drag.ix, fraction) {
            cx.emit(PaneGroupEvent::LayoutChanged);
            cx.notify();
        }
    }

    fn render_member(&self, member: &Member<Entity<Pane>>, cx: &App) -> AnyElement {
        let border = cx.theme().colors.border;
        match member {
            Member::Pane(pane) => {
                let drawn = self.drawn.clone();
                let id = pane.entity_id();
                div()
                    .relative()
                    .size_full()
                    .child(pane.clone())
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                drawn.borrow_mut().panes.insert(id, bounds);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .into_any_element()
            }
            Member::Split(split) => {
                let horizontal = split.axis == Axis::Horizontal;
                let last = split.members.len().saturating_sub(1);
                let split_id = split.id;
                let children = split.members.iter().enumerate().map(|(ix, member)| {
                    let fraction = relative(split.fraction(ix));
                    div()
                        .relative()
                        .flex_none()
                        .min_w_0()
                        .min_h_0()
                        .map(|this| {
                            if horizontal {
                                this.h_full().w(fraction)
                            } else {
                                this.w_full().h(fraction)
                            }
                        })
                        .when(ix > 0, |this| {
                            if horizontal {
                                this.border_l_1()
                            } else {
                                this.border_t_1()
                            }
                        })
                        .border_color(border)
                        .child(self.render_member(member, cx))
                        .when(ix < last, |this| {
                            this.child(
                                resize_handle::<SplitDrag, SplitDrag>(
                                    ("pane-split", (split_id.0 as usize) * 64 + ix),
                                    split.axis,
                                )
                                .inside(HandleEdge::Trailing)
                                .on_drag(
                                    SplitDrag {
                                        split: split_id,
                                        ix,
                                    },
                                    |drag, _, _, cx| cx.new(|_| *drag),
                                ),
                            )
                        })
                        .into_any_element()
                });
                let drawn = self.drawn.clone();
                let container = if horizontal { h_flex() } else { v_flex() };
                container
                    .relative()
                    .size_full()
                    .children(children)
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                drawn.borrow_mut().splits.insert(split_id, bounds);
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .into_any_element()
            }
        }
    }
}

/// Replaces the panes of `member` with their index in `panes`.
fn index_panes(member: Member<SerializedPane>, panes: &mut Vec<SerializedPane>) -> Member<usize> {
    match member {
        Member::Pane(pane) => {
            panes.push(pane);
            Member::Pane(panes.len() - 1)
        }
        Member::Split(split) => Member::Split(Split {
            id: split.id,
            axis: split.axis,
            members: split
                .members
                .into_iter()
                .map(|m| index_panes(m, panes))
                .collect(),
            flexes: split.flexes,
        }),
    }
}

impl Render for PaneGroup {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div()
            .id("pane-group")
            .debug_selector(|| "pane-group".into())
            .size_full()
            .on_drag_move(cx.listener(Self::on_split_drag))
            .child(self.render_member(self.tree.root(), cx))
    }
}

#[cfg(test)]
mod tests;

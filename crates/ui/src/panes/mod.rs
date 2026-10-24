//! Tabs and splits for the center of a window, the way Zed does them (ADR-018).
//!
//! - A [`PaneGroup`] lays out [`Pane`]s in horizontal and vertical splits
//!   ([`tree::PaneTree`]); the split ratios are dragged with handles between panes.
//! - A [`Pane`] has a tab bar and shows its active tab's [`Item`].
//! - Tabs move by drag and drop: onto a tab bar (reorder, or into another pane) or onto
//!   a pane's content (its edges split it). Keyboard: [`SplitRight`] and friends,
//!   [`CloseActiveItem`], [`ActivateNextItem`], [`ActivateItem`], [`ActivatePaneLeft`]…
//! - The layout is written as a [`SerializedLayout`] and restored through registered
//!   [`ItemKind`]s. Tabs that aren't shown are restored lazily ([`PendingItem`]).
//!
//! The group knows nothing about mail: Maze's main window puts mail views, messages
//! and drafts in it (`mail_ui`, `composer`).

mod group;
pub mod item;
mod pane;
pub mod tree;

use gpui::{Action, App, Entity, InteractiveElement, Window, actions};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use group::{PaneGroup, PaneGroupEvent, SplitFallback};
pub use item::{
    Item, ItemEvent, ItemHandle, ItemKind, PendingItem, Restore, RestoreFn, SerializedItem, TitleFn,
};
pub use pane::{DraggedTab, DropTarget, EmptyState, Pane, PaneEvent, TAB_BAR_HEIGHT};
pub use tree::SplitDirection;

actions!(
    pane,
    [
        /// Close the active tab (`cmd-w`); a draft with unsaved changes asks first.
        CloseActiveItem,
        /// Activate the next tab (`ctrl-tab`, `cmd-shift-]`).
        ActivateNextItem,
        /// Activate the previous tab (`ctrl-shift-tab`, `cmd-shift-[`).
        ActivatePreviousItem,
        /// Activate the last tab (`cmd-9`).
        ActivateLastItem,
        /// Split the active pane; the new pane goes left (`cmd-ctrl-left`).
        SplitLeft,
        /// Split the active pane; the new pane goes right (`cmd-\`, `cmd-ctrl-right`).
        SplitRight,
        /// Split the active pane; the new pane goes above (`cmd-ctrl-up`).
        SplitUp,
        /// Split the active pane; the new pane goes below (`cmd-ctrl-down`).
        SplitDown,
    ]
);

actions!(
    workspace,
    [
        /// Focus the pane to the left (`cmd-alt-left`).
        ActivatePaneLeft,
        /// Focus the pane to the right (`cmd-alt-right`).
        ActivatePaneRight,
        /// Focus the pane above (`cmd-alt-up`).
        ActivatePaneUp,
        /// Focus the pane below (`cmd-alt-down`).
        ActivatePaneDown,
    ]
);

/// Activate tab N of the active pane, counted from 0 (`cmd-1` … `cmd-8`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema, Action)]
#[action(namespace = pane)]
pub struct ActivateItem(pub usize);

/// The layout of a [`PaneGroup`] as `ui_state.json` keeps it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SerializedLayout {
    /// The splits and panes.
    pub root: SerializedMember,
    /// The active pane, counted depth first (left to right, top to bottom).
    #[serde(default)]
    pub active_pane: usize,
}

/// A pane or a split in a [`SerializedLayout`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedMember {
    /// A pane.
    Pane(SerializedPane),
    /// A split.
    Split {
        /// Side by side or stacked.
        axis: SerializedAxis,
        /// The members (at least two).
        members: Vec<SerializedMember>,
        /// Relative sizes, one per member.
        #[serde(default)]
        flexes: Vec<f32>,
    },
}

/// The tabs of a pane.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SerializedPane {
    /// The tabs, in order.
    #[serde(default)]
    pub items: Vec<SerializedItem>,
    /// The active tab.
    #[serde(default)]
    pub active: usize,
}

/// The direction of a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SerializedAxis {
    /// Members side by side.
    Horizontal,
    /// Members stacked.
    Vertical,
}

impl From<gpui::Axis> for SerializedAxis {
    fn from(axis: gpui::Axis) -> Self {
        match axis {
            gpui::Axis::Horizontal => Self::Horizontal,
            gpui::Axis::Vertical => Self::Vertical,
        }
    }
}

impl From<SerializedAxis> for gpui::Axis {
    fn from(axis: SerializedAxis) -> Self {
        match axis {
            SerializedAxis::Horizontal => Self::Horizontal,
            SerializedAxis::Vertical => Self::Vertical,
        }
    }
}

/// Handles the pane actions on `element` by forwarding them to `group`. Put it on the
/// window's root, so the actions work from anywhere in the window, the command palette
/// included.
pub fn bind_actions<E: InteractiveElement>(element: E, group: &Entity<PaneGroup>) -> E {
    fn on<A: Action, E: InteractiveElement>(
        element: E,
        group: &Entity<PaneGroup>,
        f: impl Fn(&mut PaneGroup, &A, &mut Window, &mut gpui::Context<'_, PaneGroup>) + 'static,
    ) -> E {
        let group = group.downgrade();
        element.on_action(move |action: &A, window: &mut Window, cx: &mut App| {
            group
                .update(cx, |group, cx| f(group, action, window, cx))
                .ok();
        })
    }
    let element = on(element, group, |g, _: &CloseActiveItem, w, cx| {
        g.close_active_item(w, cx)
    });
    let element = on(element, group, |g, _: &ActivateNextItem, w, cx| {
        g.activate_next_item(w, cx)
    });
    let element = on(element, group, |g, _: &ActivatePreviousItem, w, cx| {
        g.activate_previous_item(w, cx)
    });
    let element = on(element, group, |g, _: &ActivateLastItem, w, cx| {
        g.activate_last_item(w, cx)
    });
    let element = on(element, group, |g, a: &ActivateItem, w, cx| {
        g.activate_item_at(a.0, w, cx)
    });
    let element = on(element, group, |g, _: &SplitLeft, w, cx| {
        g.split(SplitDirection::Left, w, cx)
    });
    let element = on(element, group, |g, _: &SplitRight, w, cx| {
        g.split(SplitDirection::Right, w, cx)
    });
    let element = on(element, group, |g, _: &SplitUp, w, cx| {
        g.split(SplitDirection::Up, w, cx)
    });
    let element = on(element, group, |g, _: &SplitDown, w, cx| {
        g.split(SplitDirection::Down, w, cx)
    });
    let element = on(element, group, |g, _: &ActivatePaneLeft, w, cx| {
        g.activate_pane_in_direction(SplitDirection::Left, w, cx)
    });
    let element = on(element, group, |g, _: &ActivatePaneRight, w, cx| {
        g.activate_pane_in_direction(SplitDirection::Right, w, cx)
    });
    let element = on(element, group, |g, _: &ActivatePaneUp, w, cx| {
        g.activate_pane_in_direction(SplitDirection::Up, w, cx)
    });
    on(element, group, |g, _: &ActivatePaneDown, w, cx| {
        g.activate_pane_in_direction(SplitDirection::Down, w, cx)
    })
}

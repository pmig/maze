# ADR-018: Workspace panes, tabs and splits

- **Status:** Accepted
- **Date:** 2026-10-24

## Context

The main window had fixed columns (CONCEPT §8.2): spaces bar, folder pane, message list,
message view, agent column. Writing mail opened a separate window (`composer`), which
sat badly with the agent column: the draft lived outside the window whose agent should
help with it, and several drafts were several windows titled "New Message".

What we want, modelled on Zed's workspace:

- The center becomes a group of panes in horizontal and vertical splits. Each pane has
  a tab bar; tabs are a mail view (list and message of a folder), a single message, or
  a draft. The folder pane stays left (toggled from the title bar or `cmd-b`), the
  agent column stays right, outside the splits.
- Tabs are dragged within a tab bar, to another pane, or onto a pane's edge to split
  it, with the drop zone highlighted. Keyboard: split, close, next/previous tab,
  tab N, focus the pane in a direction.
- A draft's tab shows a dot while it has unsaved changes and asks before closing; its
  title is a random two-word name (`brave-otter`, like Zed's worktree names), stored
  with the draft.
- The layout (splits, ratios, tabs, active tab per pane) survives restarts, without
  writing mail content to disk (PLAN §4.17).

Two ways to build it were on the table:

1. **gpui-component's dock** (`gpui_component::dock` over `gpui_base::dock`, 0.8):
   Longbridge's Zed-inspired `DockArea` with a pane tree, tab groups, drag and drop into
   splits, resize handles, and serialization through a global `PanelRegistry`.
2. **Our own pane group**, modelled on Zed's `workspace` crate (`pane_group.rs`,
   `pane.rs`, `item.rs`; GPL-3.0 like Maze).

The dock does a lot: tab drag and drop between groups, edge drops that split, empty
groups that close, resizable splits, and a persisted state. Where it falls short for
us:

- **No close confirmation or dirty state.** A panel is `closable` or not (a plain
  `bool`); closing removes it at once. A draft with unsaved changes has to ask "Save /
  Don't Save / Cancel" and close later, or not at all. Working around it means a custom
  `TabGroupRenderer` (our own tab bar after all) plus intercepting the dock's own close
  action.
- **Restoring drops nothing silently.** A panel the registry can't build becomes an
  "invalid panel" stand-in. A tab whose message or draft was deleted should just be
  gone, and a draft tab has to wait until the core has opened the store (the window
  opens before that, PLAN §4.17).
- **Size and churn.** It is ~14,000 lines across two crates with docks, zoom, locking,
  collapse and its own active-panel state machine, of which we'd use the center only.
  Its API was just rewritten in 0.8 (the module docs say so); the renderer seam
  shifts presentation onto the host anyway.
- **Action routing and focus** are ours either way: mail actions from keys, menus and
  the command palette must reach the focused tab, and the agent column follows it.

## Decision

Maze has its own small pane group in the `ui` crate (`ui::panes`, ~2,400 lines plus
tests), modelled on Zed's:

- `tree::PaneTree<P>`: the layout as plain data (splits with relative sizes, panes as
  leaves), generic over the pane type so splitting, removing, resizing and "the pane
  to the left" are unit-tested without GPUI. A split along the parent's axis joins it;
  across, it nests (Zed's rule).
- `Pane`: a tab bar and the active item. Tabs are dragged with a `DraggedTab`; drops on
  a tab, the bar's empty end, the pane's middle or its edges are reported to the group,
  which does every move through one method, `PaneGroup::move_item` (tests use it the
  same way).
- `Item`: the trait a tab's view implements: title, icon, tooltip, dirty state,
  `request_close` (close now, or ask and emit `ItemEvent::Close` later), `serialize`,
  and `clone_on_split` for `cmd-\`.
- `PaneGroup`: the tree of panes, the active pane (it follows the focus), keyboard
  actions (bound once on the window's root with `bind_actions`, so they work from the
  palette too), and the layout file. Items are restored through registered `ItemKind`s;
  only each pane's active tab is restored at once, the others when first shown
  (`PendingItem`), and a kind can answer "later" (drafts before the core is open) or
  "gone" (dropped silently).

The crates stay layered (AGENTS.md): `ui` knows no mail. `mail_ui` provides the mail
view (`MailView`, one `ThreadList` and `MessageView` each) and the message tab
(`MessageTab`); `composer` implements `Item` for its `Composer` (a tab when opened with
`ComposerOptions::in_tab`); the binary opens draft tabs on the window's
`ComposeRequest`s and registers how draft tabs are restored, as it plugs in the agent
panel. Mail actions are handled on the window's root and go to the active tab.

The layout is version 3 of `ui_state.json`: folder, message and draft ids plus a draft's
two-word name, never subjects. Draft names live in the store (`draft.name`, schema v7).

Key bindings follow Zed except where Thunderbird's win (the keymap's rule): `cmd-k` stays
"search all messages", so Zed's `cmd-k <arrow>` chords, which would make `cmd-k` wait a
second for a second key, become `cmd-ctrl-<arrow>` (split) and `cmd-alt-<arrow>` (focus
a pane). `cmd-\`, `cmd-w`, `ctrl-tab`, `cmd-{`/`cmd-}` and `cmd-1`…`cmd-9` are Zed's.

## Consequences

- Drafts, messages and folders sit side by side in one window, next to the agent
  column; the separate compose window remains only behind "Open in New Window".
- We own ~2,400 lines of pane code instead of tracking the dock's API; behavior we need
  later (pinned tabs, a tab's context menu, "close others", navigation history) is ours
  to add, with Zed as the reference.
- Every mail view has its own list. The list's view (cards or table, columns, sort) and
  its width stay global; a mail view lays out list and message by its own width, so a
  narrow split stacks the message below the list.
- Tabs that aren't shown cost nothing at startup: only active tabs are created, so the
  cold start to the message list is unchanged.
- The layout file must keep loading older versions (missing `layout` opens the Inbox)
  and must never gain mail content.

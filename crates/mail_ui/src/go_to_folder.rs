//! Go to Folder (`cmd-k`): a picker over every folder, like Quick Move's, that opens the
//! picked folder in the active mail view. Type part of a folder's path, pick one with
//! the arrow keys, Enter opens it; Escape cancels.
//!
//! It lists the unified folders ("Unified / Inbox"), then the favorites, then every
//! other folder of every account, each as its path with the account first
//! ("Work / Archives / 2025"). With an empty filter the folders opened from here last
//! come first (this session). Typing ranks the paths like Quick Move
//! ([`rank_paths`]): every term must occur, as a substring or as a subsequence.
//!
//! Like Quick Move it renders inside the main window's element tree; the window opens
//! the folder when the picker reports [`GoToFolderEvent::Confirmed`].

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    px,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon,
    command::{Command, CommandItem, CommandState},
};
use mazemail_types::FolderRole;
use ui::theme::scaled;

use crate::folder_pane::role_icon;
use crate::quick_move::rank_paths;
use crate::source::{FolderEntry, FolderKey, FolderTree};

/// How many folders opened from the picker it remembers.
pub const RECENT_DESTINATIONS: usize = 8;

/// One folder the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// What opening it shows.
    pub key: FolderKey,
    /// The folder's name, the title of the view that shows it: "2025".
    pub name: SharedString,
    /// Its path, account first: "Work / Archives / 2025", "Unified / Inbox".
    pub path: SharedString,
    /// Special-use role (decides the icon).
    pub role: Option<FolderRole>,
}

/// Every folder of `tree`: the unified folders, the favorites, then the other folders
/// of each account in folder pane order.
pub fn destinations(tree: &FolderTree) -> Vec<Destination> {
    fn walk(entries: &[FolderEntry], parent: &str, out: &mut Vec<Destination>) {
        for entry in entries {
            let path = format!("{parent} / {}", entry.name);
            out.push(Destination {
                key: entry.key.clone(),
                name: entry.name.clone(),
                path: path.clone().into(),
                role: entry.role,
            });
            walk(&entry.children, &path, out);
        }
    }
    let mut unified = Vec::new();
    walk(&tree.unified, "Unified", &mut unified);
    let mut folders = Vec::new();
    for account in &tree.accounts {
        walk(&account.folders, &account.name, &mut folders);
    }
    let is_favorite = |key: &FolderKey| tree.favorites.iter().any(|f| &f.key == key);
    let (favorites, rest): (Vec<_>, Vec<_>) =
        folders.into_iter().partition(|d| is_favorite(&d.key));
    unified.into_iter().chain(favorites).chain(rest).collect()
}

/// What the picker reports to the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoToFolderEvent {
    /// Open this folder.
    Confirmed(Destination),
    /// Closed without opening anything.
    Dismissed,
}

/// The Go to Folder picker.
pub struct GoToFolderPicker {
    state: Entity<CommandState>,
    destinations: Vec<Destination>,
    recent: Vec<FolderKey>,
    /// The folders for the current query, as indexes into `destinations`.
    matches: Vec<usize>,
    /// The folders of the list last rendered; the command reports confirmed rows in
    /// those coordinates.
    shown: Vec<usize>,
}

impl std::fmt::Debug for GoToFolderPicker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoToFolderPicker")
            .field("destinations", &self.destinations.len())
            .finish_non_exhaustive()
    }
}

impl GoToFolderPicker {
    /// A picker over the folders of `tree`; `recent` are the folders opened from it
    /// last, newest first.
    pub fn new(
        tree: &FolderTree,
        recent: Vec<FolderKey>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut this = Self {
            state: cx.new(|cx| CommandState::new(window, cx)),
            destinations: destinations(tree),
            recent,
            matches: Vec::new(),
            shown: Vec::new(),
        };
        this.matches = this.rank("");
        this
    }

    /// The folders offered for the current filter, in order.
    pub fn matches(&self) -> impl Iterator<Item = &Destination> {
        self.matches.iter().map(|ix| &self.destinations[*ix])
    }

    fn rank(&self, query: &str) -> Vec<usize> {
        rank_paths(
            self.destinations.iter().map(|d| d.path.as_ref()),
            query,
            |ix| {
                let key = &self.destinations[ix].key;
                self.recent
                    .iter()
                    .position(|recent| recent == key)
                    .unwrap_or(usize::MAX)
            },
        )
    }

    fn set_query(&mut self, query: &str, cx: &mut Context<'_, Self>) {
        self.matches = self.rank(query);
        cx.notify();
    }

    fn confirm(&mut self, row: usize, cx: &mut Context<'_, Self>) {
        match self.shown.get(row) {
            Some(ix) => cx.emit(GoToFolderEvent::Confirmed(self.destinations[*ix].clone())),
            None => cx.emit(GoToFolderEvent::Dismissed),
        }
    }
}

impl EventEmitter<GoToFolderEvent> for GoToFolderPicker {}

impl Focusable for GoToFolderPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.focus_handle(cx)
    }
}

impl Render for GoToFolderPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let this = cx.weak_entity();
        self.shown = self.matches.clone();
        let items: Vec<CommandItem> = self
            .matches()
            .map(|destination| {
                CommandItem::new()
                    .icon(Icon::new(role_icon(destination.role)))
                    .label(destination.path.clone())
            })
            .collect();
        let dismiss = {
            let this = this.clone();
            move |cx: &mut App| {
                this.update(cx, |_, cx| cx.emit(GoToFolderEvent::Dismissed))
                    .ok();
            }
        };
        let query = this.clone();
        let confirm = this;
        let cancel = dismiss.clone();
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .justify_center()
            .pt(px(72.))
            .bg(theme.overlay)
            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| dismiss(cx))
            .child(
                div()
                    .w(scaled(480., cx))
                    .h(scaled(380., cx))
                    .occlude()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .shadow_lg()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Command::new(&self.state)
                            .bordered(false)
                            .filterable(false)
                            .placeholder("Go to a folder…")
                            .max_h(scaled(330., cx))
                            .items(items)
                            .empty(|_, _, cx| {
                                div()
                                    .p_3()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("No folder matches.")
                            })
                            .on_query(move |text, _, cx| {
                                query.update(cx, |this, cx| this.set_query(text, cx)).ok();
                            })
                            .on_confirm(move |index, _, cx| {
                                confirm
                                    .update(cx, |this, cx| this.confirm(index.row, cx))
                                    .ok();
                            })
                            .on_cancel(move |_, cx| cancel(cx)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use mazemail_types::{AccountId, FolderId, PublicId as _};

    use super::*;
    use crate::source::{AccountEntry, AccountKind};

    fn folder(
        id: i64,
        name: &str,
        role: Option<FolderRole>,
        children: Vec<FolderEntry>,
    ) -> FolderEntry {
        FolderEntry {
            key: FolderKey::Folder(FolderId::from_raw(id)),
            name: name.into(),
            role,
            children,
        }
    }

    fn tree() -> FolderTree {
        FolderTree {
            favorites: vec![folder(3, "Customers", None, Vec::new())],
            unified: vec![FolderEntry {
                key: FolderKey::Unified(FolderRole::Inbox),
                name: "Inbox".into(),
                role: Some(FolderRole::Inbox),
                children: Vec::new(),
            }],
            accounts: vec![AccountEntry {
                id: AccountId::from_raw(1),
                name: "Work".into(),
                kind: AccountKind::Imap,
                folders: vec![
                    folder(1, "Inbox", Some(FolderRole::Inbox), Vec::new()),
                    folder(
                        2,
                        "Archives",
                        Some(FolderRole::Archive),
                        vec![folder(4, "2025", None, Vec::new())],
                    ),
                    folder(3, "Customers", None, Vec::new()),
                ],
            }],
        }
    }

    fn paths(destinations: &[Destination], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .map(|ix| destinations[*ix].path.to_string())
            .collect()
    }

    #[test]
    fn unified_folders_and_favorites_come_first() {
        let all = destinations(&tree());
        let listed: Vec<&str> = all.iter().map(|d| d.path.as_ref()).collect();
        assert_eq!(
            listed,
            [
                "Unified / Inbox",
                "Work / Customers",
                "Work / Inbox",
                "Work / Archives",
                "Work / Archives / 2025",
            ]
        );
        assert_eq!(all[4].name.as_ref(), "2025");
    }

    #[test]
    fn typing_ranks_the_paths() {
        let all = destinations(&tree());
        let ranked = |query: &str| {
            paths(
                &all,
                &rank_paths(all.iter().map(|d| d.path.as_ref()), query, |_| usize::MAX),
            )
        };
        assert_eq!(ranked("inbox"), ["Unified / Inbox", "Work / Inbox"]);
        assert_eq!(ranked("arc25"), ["Work / Archives / 2025"]);
        assert_eq!(ranked("work cust"), ["Work / Customers"]);
    }
}

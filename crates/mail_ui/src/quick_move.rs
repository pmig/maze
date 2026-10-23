//! Quick Move (`shift-m`), like Thunderbird's Quick Folder Move: a small picker over the
//! folders of the selected mail's account. Type part of a folder's path, pick one with
//! the arrow keys, and Enter moves the selected rows (or thread, after `cmd-shift-a`)
//! there; Escape cancels. Mail moves within its account only.
//!
//! The picker lists every folder of the account as its path (`Archives / 2025`), except
//! the folder the mail is moved out of. With an empty filter the folders mail was moved
//! to last come first (per account, remembered in `ui_state.json`), then the rest in
//! folder pane order. Typing ranks the folders by [`rank`]: every whitespace-separated
//! term must occur in the path, as a substring or, failing that, as a subsequence
//! (`arc25` finds `Archives / 2025`).
//!
//! Like the command palette it renders inside the main window's element tree; the
//! window does the move when the picker reports [`QuickMoveEvent::Confirmed`].

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    px,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon,
    command::{Command, CommandItem, CommandState},
};
use mazemail_types::{AccountId, FolderId, FolderRole};

use crate::folder_pane::role_icon;
use crate::source::{AccountEntry, FolderEntry, FolderKey, ThreadScope};
use crate::thread_list::ItemId;

/// One folder the picker offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderChoice {
    /// The folder.
    pub id: FolderId,
    /// Its path from the account's top level: "Archives / 2025".
    pub path: SharedString,
    /// Special-use role (decides the icon).
    pub role: Option<FolderRole>,
}

/// The folders of `account` in folder pane order (parents before their children), as
/// paths, without `exclude`.
pub fn folder_choices(account: &AccountEntry, exclude: Option<FolderId>) -> Vec<FolderChoice> {
    fn walk(
        entries: &[FolderEntry],
        parent: Option<&str>,
        exclude: Option<FolderId>,
        out: &mut Vec<FolderChoice>,
    ) {
        for entry in entries {
            let path = match parent {
                Some(parent) => format!("{parent} / {}", entry.name),
                None => entry.name.to_string(),
            };
            if let FolderKey::Folder(id) = entry.key
                && Some(id) != exclude
            {
                out.push(FolderChoice {
                    id,
                    path: path.clone().into(),
                    role: entry.role,
                });
            }
            walk(&entry.children, Some(&path), exclude, out);
        }
    }
    let mut out = Vec::new();
    walk(&account.folders, None, exclude, &mut out);
    out
}

/// The folder of `account` that a list of `scope` shows mail from: the folder itself,
/// the account's folder with the role of a unified folder, or none for search results.
/// Quick Move moves out of it and doesn't offer it.
pub fn source_folder(scope: &ThreadScope, account: &AccountEntry) -> Option<FolderId> {
    fn with_role(entries: &[FolderEntry], role: FolderRole) -> Option<FolderId> {
        entries.iter().find_map(|entry| match entry.key {
            FolderKey::Folder(id) if entry.role == Some(role) => Some(id),
            _ => with_role(&entry.children, role),
        })
    }
    match scope {
        ThreadScope::Folder(FolderKey::Folder(id)) => Some(*id),
        ThreadScope::Folder(FolderKey::Unified(role)) => with_role(&account.folders, *role),
        ThreadScope::AllFolders => None,
    }
}

/// How well `term` (lowercase) matches a folder: 0 if its name starts with it, 1 if a
/// word of the path does, 2 for a substring anywhere, 3 for a subsequence; `None` if it
/// doesn't match.
fn term_score(path: &str, name: &str, term: &str) -> Option<u32> {
    if name.starts_with(term) {
        return Some(0);
    }
    if let Some(at) = path.find(term) {
        let word_start = path.match_indices(term).any(|(ix, _)| {
            path[..ix]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        });
        return Some(if word_start || at == 0 { 1 } else { 2 });
    }
    let mut rest = path.chars();
    term.chars()
        .all(|wanted| rest.any(|c| c == wanted))
        .then_some(3)
}

/// The order the picker shows `choices` in for `query`, as indexes into `choices`.
///
/// An empty query lists the `recent` targets (newest first) before the rest, in their
/// given order. Otherwise only folders matching every term are listed, best first (see
/// [`term_score`]), ties broken by recency and then by the given order.
pub fn rank(choices: &[FolderChoice], query: &str, recent: &[FolderId]) -> Vec<usize> {
    let recency = |choice: &FolderChoice| {
        recent
            .iter()
            .position(|id| *id == choice.id)
            .unwrap_or(usize::MAX)
    };
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut ranked: Vec<(u32, usize, usize)> = choices
        .iter()
        .enumerate()
        .filter_map(|(ix, choice)| {
            let path = choice.path.to_lowercase();
            let name = path.rsplit(" / ").next().unwrap_or(&path);
            let mut score = 0;
            for term in &terms {
                score += term_score(&path, name, term)?;
            }
            Some((score, recency(choice), ix))
        })
        .collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, _, ix)| ix).collect()
}

/// What the picker moves: the items, their account and the folder they leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveTarget {
    /// The selected rows, or the whole thread.
    pub items: Vec<ItemId>,
    /// Their account; the picker offers this account's folders.
    pub account: AccountId,
    /// The folder they are moved out of (see [`source_folder`]).
    pub from: Option<FolderId>,
}

/// What the picker reports to the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickMoveEvent {
    /// Move the target into this folder.
    Confirmed(FolderId),
    /// Closed without moving.
    Dismissed,
}

/// The Quick Move picker.
pub struct QuickMove {
    state: Entity<CommandState>,
    target: MoveTarget,
    account_name: SharedString,
    choices: Vec<FolderChoice>,
    recent: Vec<FolderId>,
    /// The folders for the current query, as indexes into `choices`.
    matches: Vec<usize>,
    /// The folders of the list last rendered; the command reports confirmed rows in
    /// those coordinates.
    shown: Vec<FolderId>,
}

impl std::fmt::Debug for QuickMove {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickMove")
            .field("target", &self.target)
            .field("choices", &self.choices.len())
            .finish_non_exhaustive()
    }
}

impl QuickMove {
    /// A picker over `account`'s folders for moving `target`; `recent` are the
    /// account's last move targets, newest first.
    pub fn new(
        target: MoveTarget,
        account: &AccountEntry,
        recent: Vec<FolderId>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let choices = folder_choices(account, target.from);
        let matches = rank(&choices, "", &recent);
        Self {
            state: cx.new(|cx| CommandState::new(window, cx)),
            target,
            account_name: account.name.clone(),
            choices,
            recent,
            matches,
            shown: Vec::new(),
        }
    }

    /// What the picker moves.
    pub fn target(&self) -> &MoveTarget {
        &self.target
    }

    /// The folders offered for the current filter, in order.
    pub fn matches(&self) -> impl Iterator<Item = &FolderChoice> {
        self.matches.iter().map(|ix| &self.choices[*ix])
    }

    fn set_query(&mut self, query: &str, cx: &mut Context<'_, Self>) {
        self.matches = rank(&self.choices, query, &self.recent);
        cx.notify();
    }

    fn confirm(&mut self, row: usize, cx: &mut Context<'_, Self>) {
        match self.shown.get(row) {
            Some(folder) => cx.emit(QuickMoveEvent::Confirmed(*folder)),
            None => cx.emit(QuickMoveEvent::Dismissed),
        }
    }
}

impl EventEmitter<QuickMoveEvent> for QuickMove {}

impl Focusable for QuickMove {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.focus_handle(cx)
    }
}

impl Render for QuickMove {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let this = cx.weak_entity();
        self.shown = self.matches().map(|choice| choice.id).collect();
        let items: Vec<CommandItem> = self
            .matches()
            .map(|choice| {
                CommandItem::new()
                    .icon(Icon::new(role_icon(choice.role)))
                    .label(choice.path.clone())
            })
            .collect();
        let dismiss = {
            let this = this.clone();
            move |cx: &mut App| {
                this.update(cx, |_, cx| cx.emit(QuickMoveEvent::Dismissed))
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
                    .w(px(420.))
                    .h(px(340.))
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
                            .placeholder(format!("Move to a folder in {}…", self.account_name))
                            .max_h(px(290.))
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
    use super::*;
    use crate::source::AccountKind;

    fn folder(
        id: i64,
        name: &str,
        role: Option<FolderRole>,
        children: Vec<FolderEntry>,
    ) -> FolderEntry {
        FolderEntry {
            key: FolderKey::Folder(FolderId(id)),
            name: name.to_owned().into(),
            role,
            children,
        }
    }

    fn account() -> AccountEntry {
        AccountEntry {
            id: AccountId(1),
            name: "Work".into(),
            kind: AccountKind::Imap,
            folders: vec![
                folder(1, "Inbox", Some(FolderRole::Inbox), Vec::new()),
                folder(2, "Sent", Some(FolderRole::Sent), Vec::new()),
                folder(
                    3,
                    "Archives",
                    Some(FolderRole::Archive),
                    vec![
                        folder(4, "2025", None, Vec::new()),
                        folder(5, "2026", None, Vec::new()),
                    ],
                ),
                folder(
                    6,
                    "Customers",
                    None,
                    vec![folder(7, "Acme", None, Vec::new())],
                ),
                folder(8, "Receipts", None, Vec::new()),
            ],
        }
    }

    fn paths(choices: &[FolderChoice], order: &[usize]) -> Vec<String> {
        order
            .iter()
            .map(|ix| choices[*ix].path.to_string())
            .collect()
    }

    #[test]
    fn lists_paths_in_tree_order_without_the_source_folder() {
        let choices = folder_choices(&account(), Some(FolderId(1)));
        let all: Vec<usize> = (0..choices.len()).collect();
        assert_eq!(
            paths(&choices, &all),
            [
                "Sent",
                "Archives",
                "Archives / 2025",
                "Archives / 2026",
                "Customers",
                "Customers / Acme",
                "Receipts"
            ]
        );
        assert_eq!(choices[1].role, Some(FolderRole::Archive));
    }

    #[test]
    fn the_source_folder_follows_the_list() {
        let account = account();
        let folder = |key| ThreadScope::Folder(key);
        assert_eq!(
            source_folder(&folder(FolderKey::Folder(FolderId(4))), &account),
            Some(FolderId(4))
        );
        assert_eq!(
            source_folder(&folder(FolderKey::Unified(FolderRole::Inbox)), &account),
            Some(FolderId(1))
        );
        assert_eq!(
            source_folder(&folder(FolderKey::Unified(FolderRole::Junk)), &account),
            None
        );
        assert_eq!(source_folder(&ThreadScope::AllFolders, &account), None);
    }

    #[test]
    fn an_empty_filter_puts_recent_targets_first() {
        let choices = folder_choices(&account(), Some(FolderId(1)));
        let recent = [FolderId(7), FolderId(99), FolderId(4)];
        assert_eq!(
            paths(&choices, &rank(&choices, "  ", &recent)),
            [
                "Customers / Acme",
                "Archives / 2025",
                "Sent",
                "Archives",
                "Archives / 2026",
                "Customers",
                "Receipts"
            ]
        );
    }

    #[test]
    fn filters_case_insensitively_best_match_first() {
        let choices = folder_choices(&account(), Some(FolderId(1)));
        let ranked =
            |query: &str, recent: &[FolderId]| paths(&choices, &rank(&choices, query, recent));
        // The name starting with the term beats a parent's name, which beats a substring.
        assert_eq!(ranked("ACME", &[]), ["Customers / Acme"]);
        assert_eq!(
            ranked("arch", &[]),
            ["Archives", "Archives / 2025", "Archives / 2026"]
        );
        assert_eq!(ranked("202", &[]), ["Archives / 2025", "Archives / 2026"]);
        // A substring beats a subsequence.
        assert_eq!(
            ranked("rs", &[]),
            [
                "Customers",
                "Customers / Acme",
                "Archives",
                "Archives / 2025",
                "Archives / 2026",
                "Receipts"
            ]
        );
        // Every term must match.
        assert_eq!(ranked("arch 26", &[]), ["Archives / 2026"]);
        // Fuzzy: a subsequence of the path.
        assert_eq!(ranked("arc25", &[]), ["Archives / 2025"]);
        assert_eq!(ranked("custacme", &[]), ["Customers / Acme"]);
        assert!(ranked("zzz", &[]).is_empty());
        // Recent targets win ties.
        assert_eq!(
            ranked("202", &[FolderId(5)]),
            ["Archives / 2026", "Archives / 2025"]
        );
    }
}

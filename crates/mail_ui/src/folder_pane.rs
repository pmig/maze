//! The folder pane: Favorites, Unified Folders and Accounts in a gpui-kit `Tree`, with
//! unread counts (CONCEPT §8.2, column 1).

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex,
    list::ListItem,
    tree::{TreeEntry, TreeItem, TreeState, tree},
    v_flex,
};
use mazemail_types::FolderRole;
use ui::assets::IconName;
use ui::components::CountBadge;

use crate::source::{AccountKind, FolderEntry, FolderKey, FolderTree, MailSource};

/// What a tree row stands for.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// "FAVORITES", "UNIFIED FOLDERS", "ACCOUNTS".
    Section,
    /// An account root.
    Account(AccountKind),
    /// A folder; selecting it opens it.
    Folder {
        key: FolderKey,
        role: Option<FolderRole>,
        /// Shown in the list header, e.g. "Inbox · Gmail (personal)".
        title: SharedString,
    },
}

/// Events from the folder pane.
#[derive(Debug, Clone, PartialEq)]
pub enum FolderPaneEvent {
    /// A folder was selected.
    Selected {
        /// The folder.
        key: FolderKey,
        /// Its title for the list header.
        title: SharedString,
    },
}

/// The folder pane view.
pub struct FolderPane {
    source: Arc<dyn MailSource>,
    tree: Entity<TreeState>,
    nodes: Rc<HashMap<SharedString, Node>>,
    selected: Option<FolderKey>,
    _subscription: Subscription,
}

impl std::fmt::Debug for FolderPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderPane").finish_non_exhaustive()
    }
}

const SECTION_FAVORITES: &str = "section:favorites";
const SECTION_UNIFIED: &str = "section:unified";
const SECTION_ACCOUNTS: &str = "section:accounts";

/// Icon for a folder role, Thunderbird's choices on Lucide.
pub fn role_icon(role: Option<FolderRole>) -> IconName {
    match role {
        Some(FolderRole::Inbox) => IconName::Inbox,
        Some(FolderRole::Drafts) => IconName::FilePen,
        Some(FolderRole::Sent) => IconName::Send,
        Some(FolderRole::Archive) => IconName::Archive,
        Some(FolderRole::Junk) => IconName::ShieldAlert,
        Some(FolderRole::Trash) => IconName::Trash,
        Some(FolderRole::Flagged | FolderRole::Important) => IconName::Flag,
        Some(FolderRole::All) => IconName::Mails,
        None => IconName::Folder,
    }
}

fn account_icon(kind: AccountKind) -> IconName {
    match kind {
        AccountKind::Gmail | AccountKind::Microsoft => IconName::Mail,
        AccountKind::Imap => IconName::Server,
        AccountKind::Local => IconName::Mailbox,
    }
}

/// Builds tree items and the id → node map from the source's folder tree.
fn build(tree: &FolderTree) -> (Vec<TreeItem>, HashMap<SharedString, Node>) {
    let mut nodes = HashMap::new();

    fn folder_item(
        prefix: &str,
        entry: &FolderEntry,
        context: Option<&str>,
        nodes: &mut HashMap<SharedString, Node>,
    ) -> TreeItem {
        let id: SharedString = format!("{prefix}{}", entry.key.tree_id()).into();
        let title = match context {
            Some(context) => format!("{} · {context}", entry.name).into(),
            None => entry.name.clone(),
        };
        nodes.insert(
            id.clone(),
            Node::Folder {
                key: entry.key.clone(),
                role: entry.role,
                title,
            },
        );
        TreeItem::new(id, entry.name.clone()).children(
            entry
                .children
                .iter()
                .map(|child| folder_item(prefix, child, context, nodes)),
        )
    }

    let mut items = Vec::new();
    if !tree.favorites.is_empty() {
        nodes.insert(SECTION_FAVORITES.into(), Node::Section);
        items.push(
            TreeItem::new(SECTION_FAVORITES, "Favorites")
                .expanded(true)
                .children(
                    tree.favorites
                        .iter()
                        .map(|f| folder_item("favorite:", f, Some("Favorites"), &mut nodes)),
                ),
        );
    }
    nodes.insert(SECTION_UNIFIED.into(), Node::Section);
    items.push(
        TreeItem::new(SECTION_UNIFIED, "Unified Folders")
            .expanded(true)
            .children(
                tree.unified
                    .iter()
                    .map(|f| folder_item("", f, Some("Unified"), &mut nodes)),
            ),
    );
    nodes.insert(SECTION_ACCOUNTS.into(), Node::Section);
    items.push(
        TreeItem::new(SECTION_ACCOUNTS, "Accounts")
            .expanded(true)
            .children(tree.accounts.iter().enumerate().map(|(ix, account)| {
                let id: SharedString = format!("account:{}", account.id).into();
                nodes.insert(id.clone(), Node::Account(account.kind));
                TreeItem::new(id, account.name.clone())
                    // The first account starts expanded, like Thunderbird after setup.
                    .expanded(ix == 0)
                    .children(
                        account
                            .folders
                            .iter()
                            .map(|f| folder_item("", f, Some(&account.name), &mut nodes)),
                    )
            })),
    );
    (items, nodes)
}

impl FolderPane {
    /// A folder pane for `source`, with `initial` selected.
    pub fn new(
        source: Arc<dyn MailSource>,
        initial: &FolderKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let (items, nodes) = build(&source.folder_tree());
        let initial_id = initial.tree_id();
        let tree = cx.new(|cx| {
            let mut state = TreeState::new(cx).items(items);
            let ix = state.index_of(&initial_id);
            state.set_selected_index(ix, cx);
            state
        });
        let nodes = Rc::new(nodes);
        let subscription = cx.observe_in(&tree, window, |this: &mut Self, tree, _, cx| {
            let Some(id) = tree.read(cx).selected_item().map(|item| item.id.clone()) else {
                return;
            };
            if let Some(Node::Folder { key, title, .. }) = this.nodes.get(&id)
                && this.selected.as_ref() != Some(key)
            {
                this.selected = Some(key.clone());
                cx.emit(FolderPaneEvent::Selected {
                    key: key.clone(),
                    title: title.clone(),
                });
            }
        });
        Self {
            source,
            tree,
            nodes,
            selected: Some(initial.clone()),
            _subscription: subscription,
        }
    }

    /// Rebuilds the tree from the source after folders or accounts were added,
    /// renamed or removed. The selected folder stays selected if it still exists;
    /// no event is emitted.
    pub fn reload(&mut self, cx: &mut Context<'_, Self>) {
        let (items, nodes) = build(&self.source.folder_tree());
        self.nodes = Rc::new(nodes);
        let selected = self.selected.as_ref().map(FolderKey::tree_id);
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            let ix = selected.and_then(|id| tree.index_of(&id));
            tree.set_selected_index(ix, cx);
        });
        cx.notify();
    }

    /// Re-renders the rows after unread counts changed.
    pub fn refresh_counts(&mut self, cx: &mut Context<'_, Self>) {
        self.tree.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// Selects a folder in the tree without emitting an event.
    pub fn select(&mut self, key: &FolderKey, cx: &mut Context<'_, Self>) {
        self.selected = Some(key.clone());
        let id = key.tree_id();
        self.tree.update(cx, |tree, cx| {
            let ix = tree.index_of(&id);
            tree.set_selected_index(ix, cx);
        });
    }

    /// The title of a folder as the list header shows it.
    pub fn title_of(&self, key: &FolderKey) -> Option<SharedString> {
        self.nodes.values().find_map(|node| match node {
            Node::Folder { key: k, title, .. } if k == key => Some(title.clone()),
            _ => None,
        })
    }

    fn render_item(
        nodes: &HashMap<SharedString, Node>,
        source: &dyn MailSource,
        ix: usize,
        entry: &TreeEntry,
        selected: bool,
        cx: &App,
    ) -> ListItem {
        let theme = cx.theme();
        let item = entry.item();
        let indent = px(10.) * entry.depth().saturating_sub(1) as f32;
        let chevron = || {
            Icon::new(if entry.is_expanded() {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .xsmall()
            .text_color(theme.muted_foreground)
        };
        let row = ListItem::new(ix).py_0p5().px_2();
        match nodes.get(&item.id) {
            Some(Node::Section) => row.mt_2().child(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child(item.label.to_uppercase()),
            ),
            Some(Node::Account(kind)) => row.child(
                h_flex()
                    .gap_1p5()
                    .pl(indent)
                    .text_sm()
                    .font_semibold()
                    .child(chevron())
                    .child(
                        Icon::new(account_icon(*kind))
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .child(div().truncate().child(item.label.clone())),
            ),
            Some(Node::Folder { key, role, .. }) => {
                let unread = source.unread_count(key);
                row.child(
                    h_flex()
                        .gap_1p5()
                        .pl(indent)
                        .text_sm()
                        .child(if entry.is_folder() {
                            chevron().into_any_element()
                        } else {
                            div().w(px(12.)).flex_none().into_any_element()
                        })
                        .child(
                            Icon::new(role_icon(*role))
                                .small()
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .when(unread > 0, |this| this.font_semibold())
                                .child(item.label.clone()),
                        )
                        .child(CountBadge::new(unread).emphasized(selected)),
                )
            }
            None => row.child(item.label.clone()),
        }
    }
}

impl EventEmitter<FolderPaneEvent> for FolderPane {}

impl Render for FolderPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let nodes = self.nodes.clone();
        let source = self.source.clone();
        v_flex()
            .size_full()
            .key_context("FolderPane")
            .bg(cx.theme().sidebar)
            .py_1()
            .child(tree(&self.tree, move |ix, entry, selected, _, cx| {
                FolderPane::render_item(&nodes, source.as_ref(), ix, entry, selected, cx)
            }))
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::fake::FakeMailSource;

    #[test]
    fn builds_sections_with_unique_ids() {
        let source = FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC));
        let (items, nodes) = build(&source.folder_tree());
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].label.as_ref(), "Favorites");
        assert_eq!(items[1].label.as_ref(), "Unified Folders");
        assert_eq!(items[2].label.as_ref(), "Accounts");
        // A favorite and its account folder share a FolderKey but not a tree id.
        let favorite = &items[0].children[0];
        assert!(favorite.id.starts_with("favorite:"));
        let Some(Node::Folder { key, .. }) = nodes.get(&favorite.id) else {
            panic!("favorite is a folder");
        };
        assert!(nodes.contains_key(&key.tree_id()));
        assert_eq!(
            nodes.get(&FolderKey::Unified(FolderRole::Inbox).tree_id()),
            Some(&Node::Folder {
                key: FolderKey::Unified(FolderRole::Inbox),
                role: Some(FolderRole::Inbox),
                title: "Inbox · Unified".into(),
            })
        );
    }

    #[test]
    fn roles_have_icons() {
        assert_eq!(role_icon(Some(FolderRole::Inbox)), IconName::Inbox);
        assert_eq!(role_icon(None), IconName::Folder);
    }
}

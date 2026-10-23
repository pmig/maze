//! The folder pane: Favorites, Unified Folders and Accounts in a gpui-kit `Tree`, with
//! unread counts (CONCEPT §8.2, column 1).
//!
//! **Favorites** come first. The source says which folders are favorites (the
//! `mazemail` binary keeps them in `settings.json`); the pane shows them in account
//! order, then folder tree order ([`favorite_groups`]), under the account's name when
//! there is more than one account, so a favorite "Inbox" says whose it is. Without
//! favorites the section says "No favorite folders yet". Right-clicking a folder
//! offers "Add to Favorites" or "Remove from Favorites"; `mail::ToggleFavoriteFolder`
//! does the same for the open folder. A favorite opens its folder like the folder's
//! row in its account.
//!
//! Sections, accounts and folders with subfolders collapse and expand; the pane
//! remembers which, so a rebuild (a new favorite, a synced folder) keeps them.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, WeakEntity, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    list::ListItem,
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
    tree::{TreeEntry, TreeEvent, TreeItem, TreeState, tree},
    v_flex,
};
use mazemail_types::{FolderId, FolderRole, PRODUCT_NAME};
use ui::assets::IconName;
use ui::components::CountBadge;

use crate::actions::AddAccount;
use crate::source::{AccountEntry, AccountKind, FolderEntry, FolderKey, FolderTree, MailSource};

/// What a tree row stands for.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// "FAVORITES", "UNIFIED FOLDERS", "ACCOUNTS".
    Section,
    /// An account root.
    Account(AccountKind),
    /// An account's name above its favorites (with more than one account).
    FavoritesAccount,
    /// The empty state of Favorites: "No favorite folders yet" and how to add one.
    NoFavorites,
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
    /// The favorite folders, for the context menu.
    favorites: Rc<HashSet<FolderKey>>,
    /// Rows the user expanded or collapsed, by tree id; a rebuild keeps them so.
    expanded: HashMap<SharedString, bool>,
    selected: Option<FolderKey>,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for FolderPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderPane").finish_non_exhaustive()
    }
}

const SECTION_FAVORITES: &str = "section:favorites";
const SECTION_UNIFIED: &str = "section:unified";
const SECTION_ACCOUNTS: &str = "section:accounts";
const NO_FAVORITES: &str = "favorites:none";
const NO_FAVORITES_HINT: &str = "favorites:hint";

/// The empty state of Favorites.
pub const NO_FAVORITES_TEXT: &str = "No favorite folders yet";
/// How to add a favorite, under [`NO_FAVORITES_TEXT`].
pub const NO_FAVORITES_HINT_TEXT: &str = "Right-click a folder to add it";

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

/// The favorites of one account, in folder tree order.
#[derive(Debug, Clone, PartialEq)]
pub struct FavoriteGroup<'a> {
    /// The account.
    pub account: &'a AccountEntry,
    /// Its favorite folders.
    pub folders: Vec<&'a FolderEntry>,
}

/// The favorites of `tree` by account: accounts in order, each account's favorites in
/// folder tree order (depth first), accounts without favorites left out. Favorites
/// that aren't in any account's tree (a folder that is gone) are left out too.
pub fn favorite_groups(tree: &FolderTree) -> Vec<FavoriteGroup<'_>> {
    fn collect<'a>(
        entries: &'a [FolderEntry],
        favorites: &HashSet<&FolderKey>,
        out: &mut Vec<&'a FolderEntry>,
    ) {
        for entry in entries {
            if favorites.contains(&entry.key) {
                out.push(entry);
            }
            collect(&entry.children, favorites, out);
        }
    }
    let favorites: HashSet<&FolderKey> = tree.favorites.iter().map(|f| &f.key).collect();
    tree.accounts
        .iter()
        .filter_map(|account| {
            let mut folders = Vec::new();
            collect(&account.folders, &favorites, &mut folders);
            (!folders.is_empty()).then_some(FavoriteGroup { account, folders })
        })
        .collect()
}

/// Whether Favorites shows account names above the favorites: with more than one
/// account, whether or not the favorites are from several (an "Inbox" favorite is
/// ambiguous otherwise, and the section doesn't change shape with the next favorite).
pub fn groups_favorites_by_account(tree: &FolderTree) -> bool {
    tree.accounts.len() > 1
}

/// What the context menu of a row toggles: the folder, and whether it is a favorite
/// now. `None` for rows that aren't a folder of an account (unified folders can't be
/// favorites).
fn favorite_toggle(
    node: Option<&Node>,
    favorites: &HashSet<FolderKey>,
) -> Option<(FolderId, bool)> {
    match node {
        Some(Node::Folder {
            key: key @ FolderKey::Folder(folder),
            ..
        }) => Some((*folder, favorites.contains(key))),
        _ => None,
    }
}

/// The tree items, the id → node map and the favorite keys, from the source's folder
/// tree. `expanded` holds the rows the user expanded or collapsed.
fn build(
    tree: &FolderTree,
    expanded: &HashMap<SharedString, bool>,
) -> (
    Vec<TreeItem>,
    HashMap<SharedString, Node>,
    HashSet<FolderKey>,
) {
    let mut nodes = HashMap::new();
    let is_expanded =
        |id: &SharedString, default: bool| expanded.get(id).copied().unwrap_or(default);

    fn folder_item(
        entry: &FolderEntry,
        context: &str,
        nodes: &mut HashMap<SharedString, Node>,
        expanded: &HashMap<SharedString, bool>,
    ) -> TreeItem {
        let id = entry.key.tree_id();
        nodes.insert(
            id.clone(),
            Node::Folder {
                key: entry.key.clone(),
                role: entry.role,
                title: format!("{} · {context}", entry.name).into(),
            },
        );
        TreeItem::new(id.clone(), entry.name.clone())
            .expanded(expanded.get(&id).copied().unwrap_or(false))
            .children(
                entry
                    .children
                    .iter()
                    .map(|child| folder_item(child, context, nodes, expanded)),
            )
    }

    let mut items = Vec::new();

    // Favorites: flat (no subfolders), titled like the folder in its account.
    let favorite_item =
        |entry: &FolderEntry, account: &AccountEntry, nodes: &mut HashMap<SharedString, Node>| {
            let id: SharedString = format!("favorite:{}", entry.key.tree_id()).into();
            nodes.insert(
                id.clone(),
                Node::Folder {
                    key: entry.key.clone(),
                    role: entry.role,
                    title: format!("{} · {}", entry.name, account.name).into(),
                },
            );
            TreeItem::new(id, entry.name.clone())
        };
    let groups = favorite_groups(tree);
    let favorites: HashSet<FolderKey> = groups
        .iter()
        .flat_map(|group| group.folders.iter().map(|f| f.key.clone()))
        .collect();
    let favorite_items: Vec<TreeItem> = if groups.is_empty() {
        nodes.insert(NO_FAVORITES.into(), Node::NoFavorites);
        nodes.insert(NO_FAVORITES_HINT.into(), Node::NoFavorites);
        vec![
            TreeItem::new(NO_FAVORITES, NO_FAVORITES_TEXT).disabled(true),
            TreeItem::new(NO_FAVORITES_HINT, NO_FAVORITES_HINT_TEXT).disabled(true),
        ]
    } else if groups_favorites_by_account(tree) {
        groups
            .iter()
            .map(|group| {
                let id: SharedString = format!("favorites:account:{}", group.account.id).into();
                nodes.insert(id.clone(), Node::FavoritesAccount);
                TreeItem::new(id.clone(), group.account.name.clone())
                    .expanded(is_expanded(&id, true))
                    .children(
                        group
                            .folders
                            .iter()
                            .map(|f| favorite_item(f, group.account, &mut nodes)),
                    )
            })
            .collect()
    } else {
        groups
            .iter()
            .flat_map(|group| group.folders.iter().map(|f| (*f, group.account)))
            .map(|(f, account)| favorite_item(f, account, &mut nodes))
            .collect()
    };
    let section: SharedString = SECTION_FAVORITES.into();
    nodes.insert(section.clone(), Node::Section);
    items.push(
        TreeItem::new(section.clone(), "Favorites")
            .expanded(is_expanded(&section, true))
            .children(favorite_items),
    );

    let section: SharedString = SECTION_UNIFIED.into();
    nodes.insert(section.clone(), Node::Section);
    items.push(
        TreeItem::new(section.clone(), "Unified Folders")
            .expanded(is_expanded(&section, true))
            .children(
                tree.unified
                    .iter()
                    .map(|f| folder_item(f, "Unified", &mut nodes, expanded)),
            ),
    );
    let section: SharedString = SECTION_ACCOUNTS.into();
    nodes.insert(section.clone(), Node::Section);
    items.push(
        TreeItem::new(section.clone(), "Accounts")
            .expanded(is_expanded(&section, true))
            .children(tree.accounts.iter().enumerate().map(|(ix, account)| {
                let id: SharedString = format!("account:{}", account.id).into();
                nodes.insert(id.clone(), Node::Account(account.kind));
                // The first account starts expanded, like Thunderbird after setup.
                TreeItem::new(id.clone(), account.name.clone())
                    .expanded(is_expanded(&id, ix == 0))
                    .children(
                        account
                            .folders
                            .iter()
                            .map(|f| folder_item(f, &account.name, &mut nodes, expanded)),
                    )
            })),
    );
    (items, nodes, favorites)
}

impl FolderPane {
    /// A folder pane for `source`, with `initial` selected.
    pub fn new(
        source: Arc<dyn MailSource>,
        initial: &FolderKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let expanded = HashMap::new();
        let (items, nodes, favorites) = build(&source.folder_tree(), &expanded);
        let initial_id = initial.tree_id();
        let tree = cx.new(|cx| {
            let mut state = TreeState::new(cx).items(items);
            let ix = state.index_of(&initial_id);
            state.set_selected_index(ix, cx);
            state
        });
        let selection = cx.observe_in(&tree, window, |this: &mut Self, tree, _, cx| {
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
        let expansion = cx.subscribe(&tree, |this: &mut Self, _, event: &TreeEvent, _| {
            let (id, open) = match event {
                TreeEvent::Expanded(id) => (id, true),
                TreeEvent::Collapsed(id) => (id, false),
            };
            this.expanded.insert(id.clone(), open);
        });
        Self {
            source,
            tree,
            nodes: Rc::new(nodes),
            favorites: Rc::new(favorites),
            expanded,
            selected: Some(initial.clone()),
            _subscriptions: vec![selection, expansion],
        }
    }

    /// Rebuilds the tree from the source after folders, accounts or favorites were
    /// added, renamed or removed. The selected row stays selected if it still exists
    /// (else the selected folder's row in its account); no event is emitted.
    pub fn reload(&mut self, cx: &mut Context<'_, Self>) {
        let (items, nodes, favorites) = build(&self.source.folder_tree(), &self.expanded);
        self.nodes = Rc::new(nodes);
        self.favorites = Rc::new(favorites);
        let selected_row = self
            .tree
            .read(cx)
            .selected_item()
            .map(|item| item.id.clone());
        let selected = self.selected.as_ref().map(FolderKey::tree_id);
        self.tree.update(cx, |tree, cx| {
            tree.set_items(items, cx);
            let ix = selected_row
                .and_then(|id| tree.index_of(&id))
                .or_else(|| selected.and_then(|id| tree.index_of(&id)));
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

    /// Whether `key` is shown under Favorites.
    pub fn is_favorite(&self, key: &FolderKey) -> bool {
        self.favorites.contains(key)
    }

    /// The favorites as shown, in order: (tree id, label). Account names above the
    /// favorites and the empty state are rows too.
    pub fn favorite_rows(&self, cx: &App) -> Vec<(SharedString, SharedString)> {
        let tree = self.tree.read(cx);
        let Some(section) = tree.index_of(&SECTION_FAVORITES.into()) else {
            return Vec::new();
        };
        (section + 1..)
            .map_while(|ix| tree.entry(ix))
            .take_while(|entry| entry.depth() > 0)
            .map(|entry| (entry.item().id.clone(), entry.item().label.clone()))
            .collect()
    }

    /// Adds `folder` to the favorites or removes it, through the source, and shows the
    /// result. A failure (the settings file can't be written) is a notification.
    pub fn set_favorite(
        &mut self,
        folder: FolderId,
        favorite: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match self.source.set_favorite(folder, favorite) {
            Ok(()) => self.reload(cx),
            Err(error) => {
                tracing::warn!(%error, favorite, "changing a favorite folder failed");
                window.push_notification(
                    Notification::error(format!("The favorite folders couldn't be saved: {error}"))
                        .title(PRODUCT_NAME),
                    cx,
                );
            }
        }
    }

    /// The context menu of a row: "Add to Favorites" or "Remove from Favorites" on
    /// folders of an account.
    fn context_menu(
        menu: PopupMenu,
        node: Option<&Node>,
        favorites: &HashSet<FolderKey>,
        pane: &WeakEntity<Self>,
    ) -> PopupMenu {
        let Some((folder, favorite)) = favorite_toggle(node, favorites) else {
            return menu;
        };
        let (label, icon) = if favorite {
            ("Remove from Favorites", IconName::StarOff)
        } else {
            ("Add to Favorites", IconName::StarPlus)
        };
        let pane = pane.clone();
        menu.item(
            PopupMenuItem::new(label)
                .icon(Icon::new(icon))
                .on_click(move |_, window, cx| {
                    pane.update(cx, |pane, cx| {
                        pane.set_favorite(folder, !favorite, window, cx)
                    })
                    .ok();
                }),
        )
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
            Some(Node::FavoritesAccount) => row.child(
                h_flex()
                    .gap_1()
                    .pl(indent)
                    .text_xs()
                    .font_medium()
                    .text_color(theme.muted_foreground)
                    .child(chevron())
                    .child(div().truncate().child(item.label.clone())),
            ),
            Some(Node::NoFavorites) => row.child(
                div()
                    .pl(indent + px(4.))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(item.label.clone()),
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
                        .debug_selector(|| format!("folder-pane-row:{}", item.id))
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
        let menu_nodes = self.nodes.clone();
        let favorites = self.favorites.clone();
        let pane = cx.weak_entity();
        let source = self.source.clone();
        let theme = cx.theme();
        v_flex()
            .size_full()
            .key_context("FolderPane")
            .bg(theme.sidebar)
            .pt_1()
            .child(
                div().flex_1().min_h_0().child(
                    tree(&self.tree, move |ix, entry, selected, _, cx| {
                        FolderPane::render_item(&nodes, source.as_ref(), ix, entry, selected, cx)
                    })
                    .context_menu(move |_, entry, menu, _, _| {
                        let node = menu_nodes.get(&entry.item().id);
                        FolderPane::context_menu(menu, node, &favorites, &pane)
                    }),
                ),
            )
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("folder-pane-add-account")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .label("Add Account…")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(AddAccount), cx);
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext};
    use time::macros::datetime;

    use super::*;
    use crate::fake::FakeMailSource;

    fn fake() -> FakeMailSource {
        FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC))
    }

    fn entry(id: i64, name: &str, children: Vec<FolderEntry>) -> FolderEntry {
        FolderEntry {
            key: FolderKey::Folder(FolderId(id)),
            name: name.to_owned().into(),
            role: None,
            children,
        }
    }

    fn account(id: i64, name: &str, folders: Vec<FolderEntry>) -> AccountEntry {
        AccountEntry {
            id: mazemail_types::AccountId(id),
            name: name.to_owned().into(),
            kind: AccountKind::Imap,
            folders,
        }
    }

    /// Two accounts: Work (1 Inbox, 2 Customers / 3 Acme, 4 To Do) and Home (5 Inbox).
    fn two_accounts(favorites: &[i64]) -> FolderTree {
        let accounts = vec![
            account(
                1,
                "Work",
                vec![
                    entry(1, "Inbox", vec![]),
                    entry(2, "Customers", vec![entry(3, "Acme", vec![])]),
                    entry(4, "To Do", vec![]),
                ],
            ),
            account(2, "Home", vec![entry(5, "Inbox", vec![])]),
        ];
        FolderTree {
            favorites: favorites
                .iter()
                .map(|id| entry(*id, "favorite", vec![]))
                .collect(),
            unified: Vec::new(),
            accounts,
        }
    }

    /// The tree id of an account row.
    fn account_id(id: i64) -> SharedString {
        format!("account:{}", mazemail_types::AccountId(id)).into()
    }

    fn ids(groups: &[FavoriteGroup<'_>]) -> Vec<(i64, Vec<i64>)> {
        groups
            .iter()
            .map(|g| {
                let folders = g
                    .folders
                    .iter()
                    .map(|f| match f.key {
                        FolderKey::Folder(id) => id.0,
                        FolderKey::Unified(_) => unreachable!(),
                    })
                    .collect();
                (g.account.id.0, folders)
            })
            .collect()
    }

    fn labels(items: &[TreeItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_ref()).collect()
    }

    #[test]
    fn favorites_follow_account_and_tree_order() {
        // In the order they were added; a favorite that is gone (99) is hidden.
        let tree = two_accounts(&[5, 4, 99, 3, 1]);
        assert_eq!(
            ids(&favorite_groups(&tree)),
            vec![(1, vec![1, 3, 4]), (2, vec![5])]
        );
        assert_eq!(
            ids(&favorite_groups(&two_accounts(&[4]))),
            vec![(1, vec![4])]
        );
        assert!(favorite_groups(&two_accounts(&[])).is_empty());
        assert!(favorite_groups(&two_accounts(&[99])).is_empty());
    }

    #[test]
    fn favorites_come_first_grouped_by_account() {
        let (items, nodes, favorites) = build(&two_accounts(&[5, 3]), &HashMap::new());
        assert_eq!(labels(&items), ["Favorites", "Unified Folders", "Accounts"]);
        let section = &items[0];
        assert_eq!(labels(&section.children), ["Work", "Home"]);
        assert_eq!(labels(&section.children[0].children), ["Acme"]);
        assert_eq!(labels(&section.children[1].children), ["Inbox"]);
        // A favorite and its account folder share a FolderKey but not a tree id; both
        // open the folder with the same title.
        let acme = &section.children[0].children[0];
        assert!(acme.id.starts_with("favorite:"));
        assert!(!acme.is_folder(), "favorites have no subfolders");
        let key = FolderKey::Folder(FolderId(3));
        let title = |id: &SharedString| match nodes.get(id) {
            Some(Node::Folder { key: k, title, .. }) if *k == key => title.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(title(&acme.id), "Acme · Work");
        assert_eq!(title(&key.tree_id()), "Acme · Work");
        assert!(favorites.contains(&key));
        assert!(!favorites.contains(&FolderKey::Folder(FolderId(1))));
    }

    #[test]
    fn one_account_lists_favorites_flat() {
        let mut tree = two_accounts(&[4, 1]);
        tree.accounts.truncate(1);
        assert!(!groups_favorites_by_account(&tree));
        let (items, _, _) = build(&tree, &HashMap::new());
        assert_eq!(labels(&items[0].children), ["Inbox", "To Do"]);

        // Several accounts group even if the favorites are from one.
        let tree = two_accounts(&[4, 1]);
        assert!(groups_favorites_by_account(&tree));
        let (items, _, _) = build(&tree, &HashMap::new());
        assert_eq!(labels(&items[0].children), ["Work"]);
    }

    #[test]
    fn no_favorites_shows_the_empty_state() {
        for tree in [two_accounts(&[]), two_accounts(&[99])] {
            let (items, nodes, favorites) = build(&tree, &HashMap::new());
            let section = &items[0];
            assert_eq!(section.label.as_ref(), "Favorites");
            assert_eq!(
                labels(&section.children),
                [NO_FAVORITES_TEXT, NO_FAVORITES_HINT_TEXT]
            );
            assert!(section.children.iter().all(TreeItem::is_disabled));
            assert_eq!(nodes.get(&section.children[0].id), Some(&Node::NoFavorites));
            assert!(favorites.is_empty());
        }
    }

    #[test]
    fn rebuilds_keep_what_was_expanded_or_collapsed() {
        let tree = two_accounts(&[5]);
        let (items, _, _) = build(&tree, &HashMap::new());
        let accounts = &items[2];
        assert!(items[0].is_expanded() && accounts.is_expanded());
        assert!(accounts.children[0].is_expanded() && !accounts.children[1].is_expanded());
        let expanded = HashMap::from([
            (SharedString::from(SECTION_FAVORITES), false),
            (account_id(1), false),
            (account_id(2), true),
            (FolderKey::Folder(FolderId(2)).tree_id(), true),
        ]);
        let (items, _, _) = build(&tree, &expanded);
        let accounts = &items[2];
        assert!(!items[0].is_expanded());
        assert!(!accounts.children[0].is_expanded() && accounts.children[1].is_expanded());
        assert!(accounts.children[0].children[1].is_expanded(), "Customers");
    }

    #[test]
    fn builds_the_fake_sources_tree() {
        let (items, nodes, favorites) = build(&fake().folder_tree(), &HashMap::new());
        assert_eq!(items.len(), 3);
        // Gmail (personal): Receipts; Microsoft 365: Acme (under Customers), To Do.
        let section = &items[0];
        assert_eq!(
            labels(&section.children),
            ["Gmail (personal)", "Microsoft 365"]
        );
        assert_eq!(labels(&section.children[1].children), ["Acme", "To Do"]);
        assert_eq!(favorites.len(), 3);
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

    fn open(
        cx: &mut TestAppContext,
    ) -> (
        Arc<FakeMailSource>,
        Entity<FolderPane>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let fake = Arc::new(fake());
        let source: Arc<dyn MailSource> = fake.clone();
        let initial = FolderKey::Unified(FolderRole::Inbox);
        let (pane, cx) =
            cx.add_window_view(|window, cx| FolderPane::new(source, &initial, window, cx));
        (fake, pane, cx)
    }

    #[gpui::test]
    fn toggling_favorites_updates_the_section(cx: &mut TestAppContext) {
        let (_, pane, cx) = open(cx);
        let rows = |cx: &mut VisualTestContext| {
            pane.read_with(cx, |pane, cx| {
                pane.favorite_rows(cx)
                    .into_iter()
                    .map(|(_, label)| label.to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            rows(cx),
            [
                "Gmail (personal)",
                "Receipts",
                "Microsoft 365",
                "Acme",
                "To Do"
            ]
        );
        // Remove all three: the empty state.
        for id in [5, 21, 22] {
            pane.update_in(cx, |pane, window, cx| {
                pane.set_favorite(FolderId(id), false, window, cx);
            });
        }
        assert_eq!(rows(cx), [NO_FAVORITES_TEXT, NO_FAVORITES_HINT_TEXT]);
        // Add one back.
        pane.update_in(cx, |pane, window, cx| {
            pane.set_favorite(FolderId(15), true, window, cx);
        });
        assert_eq!(rows(cx), ["Microsoft 365", "Inbox"]);
        assert!(pane.read_with(cx, |pane, _| {
            pane.is_favorite(&FolderKey::Folder(FolderId(15)))
        }));
    }

    #[test]
    fn the_context_menu_toggles_folders_of_accounts() {
        let (items, nodes, favorites) = build(&two_accounts(&[3]), &HashMap::new());
        let toggle = |id: &SharedString| favorite_toggle(nodes.get(id), &favorites);
        // A favorite, both under Favorites and in its account: "Remove from Favorites".
        let acme = &items[0].children[0].children[0].id;
        assert_eq!(toggle(acme), Some((FolderId(3), true)));
        assert_eq!(
            toggle(&FolderKey::Folder(FolderId(3)).tree_id()),
            Some((FolderId(3), true))
        );
        // Any other folder: "Add to Favorites".
        assert_eq!(
            toggle(&FolderKey::Folder(FolderId(4)).tree_id()),
            Some((FolderId(4), false))
        );
        // Nothing for sections, accounts and unified folders.
        assert_eq!(toggle(&SECTION_FAVORITES.into()), None);
        assert_eq!(toggle(&account_id(1)), None);
        assert_eq!(
            favorite_toggle(
                Some(&Node::Folder {
                    key: FolderKey::Unified(FolderRole::Inbox),
                    role: None,
                    title: "Inbox".into(),
                }),
                &favorites
            ),
            None
        );
    }
}

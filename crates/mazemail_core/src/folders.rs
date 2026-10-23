//! The folder pane tree (CONCEPT §8.2, Thunderbird's folder modes).
//!
//! [`CoreHandle::folders`](crate::CoreHandle::folders) returns, in this order:
//!
//! 1. **Unified Folders**: one node per role that exists in any account (Inbox, Drafts,
//!    Sent, Archive, Junk, Trash, in that order), with counts summed over the folders
//!    of that role. Its list scope is [`ListScope::Unified`].
//! 2. **Favorites**: the folders in the setting `folder_pane.favorites`, flat, with
//!    their own counts, in account order and then folder tree order. Favorites name
//!    their folder by account label and [folder path](folder_path), so they survive
//!    re-syncs; favorites whose folder doesn't exist are left out (and kept in the
//!    settings).
//! 3. **One node per account** with its folder tree.
//!
//! Sections without children are left out. The UI decides which sections to show
//! (setting `folder_pane.modes`).

use std::collections::{HashMap, HashSet};

use mazemail_types::{AccountId, FolderId, FolderRole};
use settings::FavoriteFolder;
use store::{Account, Folder, ListScope};

/// Roles shown as unified folders, in display order.
pub const UNIFIED_ROLES: [FolderRole; 6] = [
    FolderRole::Inbox,
    FolderRole::Drafts,
    FolderRole::Sent,
    FolderRole::Archive,
    FolderRole::Junk,
    FolderRole::Trash,
];

/// What a folder pane node stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderNodeKind {
    /// The "Unified Folders" section header.
    UnifiedSection,
    /// A unified folder: every folder with this role, across accounts.
    Unified(FolderRole),
    /// The "Favorites" section header.
    FavoritesSection,
    /// An account.
    Account(AccountId),
    /// A folder or Gmail label (also used for the entries under Favorites).
    Folder(FolderId),
}

/// One node of the folder pane.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderNode {
    /// What the node stands for.
    pub kind: FolderNodeKind,
    /// The name shown.
    pub name: String,
    /// The folder role (folders and unified folders).
    pub role: Option<FolderRole>,
    /// The thread list this node opens, if any (section headers and accounts open
    /// nothing).
    pub scope: Option<ListScope>,
    /// Unread messages (summed for unified folders; 0 for headers and accounts).
    pub unread: u64,
    /// All messages (summed for unified folders; 0 for headers and accounts).
    pub total: u64,
    /// Whether the folder is a favorite.
    pub favorite: bool,
    /// Whether it is a Gmail label.
    pub is_label: bool,
    /// Child nodes.
    pub children: Vec<FolderNode>,
}

impl std::fmt::Debug for FolderNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Folder names can reveal personal information; keep them out of logs.
        f.debug_struct("FolderNode")
            .field("kind", &self.kind)
            .field("role", &self.role)
            .field("unread", &self.unread)
            .field("total", &self.total)
            .field("children", &self.children)
            .finish_non_exhaustive()
    }
}

impl FolderNode {
    fn header(kind: FolderNodeKind, name: &str, children: Vec<FolderNode>) -> Self {
        Self {
            kind,
            name: name.to_owned(),
            role: None,
            scope: None,
            unread: 0,
            total: 0,
            favorite: false,
            is_label: false,
            children,
        }
    }

    fn folder(folder: &Folder, children: Vec<FolderNode>) -> Self {
        Self {
            kind: FolderNodeKind::Folder(folder.id),
            name: folder.display_name.clone(),
            role: folder.role,
            scope: Some(ListScope::Folder(folder.id)),
            unread: folder.unread,
            total: folder.total,
            favorite: folder.favorite,
            is_label: folder.is_label,
            children,
        }
    }

    /// Visits this node and its descendants, depth first.
    pub fn walk<'a>(&'a self, visit: &mut impl FnMut(&'a FolderNode)) {
        visit(self);
        for child in &self.children {
            child.walk(visit);
        }
    }
}

/// Human-readable name of a unified folder.
pub fn unified_name(role: FolderRole) -> &'static str {
    match role {
        FolderRole::Inbox => "Inbox",
        FolderRole::Drafts => "Drafts",
        FolderRole::Sent => "Sent",
        FolderRole::Archive => "Archive",
        FolderRole::Junk => "Junk",
        FolderRole::Trash => "Trash",
        FolderRole::All => "All Mail",
        FolderRole::Flagged => "Starred",
        FolderRole::Important => "Important",
    }
}

/// Separates the folder names of a folder path.
pub const PATH_SEPARATOR: char = '/';

/// The path of `folder` within its account, the names from the top joined with `/`
/// (`Customers/Acme`): how favorites name a folder. `folders` holds the account's
/// folders. `None` if a parent is missing.
pub fn folder_path(folders: &[Folder], folder: FolderId) -> Option<String> {
    let by_id: HashMap<FolderId, &Folder> = folders.iter().map(|f| (f.id, f)).collect();
    let mut names = Vec::new();
    let mut next = Some(folder);
    while let Some(id) = next {
        // A cycle can't come from the store; don't loop forever if it did.
        if names.len() > by_id.len() {
            return None;
        }
        let folder = by_id.get(&id)?;
        names.push(folder.display_name.as_str());
        next = folder.parent;
    }
    names.reverse();
    Some(names.join(&PATH_SEPARATOR.to_string()))
}

/// The favorites of one account, by folder path.
type FavoritePaths<'a> = HashSet<&'a str>;

/// Converts a store node and its subtree, marking favorites and collecting them in
/// tree order.
fn convert(
    node: &store::FolderNode,
    parent_path: &str,
    favorite_paths: &FavoritePaths<'_>,
    favorites: &mut Vec<FolderNode>,
) -> FolderNode {
    let path = if parent_path.is_empty() {
        node.folder.display_name.clone()
    } else {
        format!("{parent_path}{PATH_SEPARATOR}{}", node.folder.display_name)
    };
    let favorite = favorite_paths.contains(path.as_str());
    if favorite {
        favorites.push(FolderNode {
            favorite: true,
            ..FolderNode::folder(&node.folder, Vec::new())
        });
    }
    let children = node
        .children
        .iter()
        .map(|child| convert(child, &path, favorite_paths, favorites))
        .collect();
    FolderNode {
        favorite,
        ..FolderNode::folder(&node.folder, children)
    }
}

/// Builds the folder pane from the store's accounts and folder trees and the favorites
/// setting.
pub(crate) fn build(
    accounts: &[Account],
    folders: &[Folder],
    trees: &[(AccountId, Vec<store::FolderNode>)],
    favorites: &[FavoriteFolder],
) -> Vec<FolderNode> {
    let mut out = Vec::new();

    let unified: Vec<FolderNode> = UNIFIED_ROLES
        .iter()
        .filter_map(|&role| {
            let with_role: Vec<&Folder> = folders.iter().filter(|f| f.role == Some(role)).collect();
            (!with_role.is_empty()).then(|| FolderNode {
                kind: FolderNodeKind::Unified(role),
                name: unified_name(role).to_owned(),
                role: Some(role),
                scope: Some(ListScope::Unified(role)),
                unread: with_role.iter().map(|f| f.unread).sum(),
                total: with_role.iter().map(|f| f.total).sum(),
                favorite: false,
                is_label: false,
                children: Vec::new(),
            })
        })
        .collect();
    if !unified.is_empty() {
        out.push(FolderNode::header(
            FolderNodeKind::UnifiedSection,
            "Unified Folders",
            unified,
        ));
    }

    let mut favorite_nodes = Vec::new();
    let mut account_nodes = Vec::new();
    for account in accounts {
        let favorite_paths: FavoritePaths<'_> = favorites
            .iter()
            .filter(|f| f.account == account.name)
            .map(|f| f.folder.as_str())
            .collect();
        let children = trees
            .iter()
            .find(|(id, _)| *id == account.id)
            .map(|(_, tree)| {
                tree.iter()
                    .map(|node| convert(node, "", &favorite_paths, &mut favorite_nodes))
                    .collect()
            })
            .unwrap_or_default();
        account_nodes.push(FolderNode::header(
            FolderNodeKind::Account(account.id),
            &account.name,
            children,
        ));
    }
    if !favorite_nodes.is_empty() {
        out.push(FolderNode::header(
            FolderNodeKind::FavoritesSection,
            "Favorites",
            favorite_nodes,
        ));
    }
    out.extend(account_nodes);
    out
}

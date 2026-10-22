//! The folder pane tree (CONCEPT §8.2, Thunderbird's folder modes).
//!
//! [`CoreHandle::folders`](crate::CoreHandle::folders) returns, in this order:
//!
//! 1. **Unified Folders**: one node per role that exists in any account (Inbox, Drafts,
//!    Sent, Archive, Junk, Trash, in that order), with counts summed over the folders
//!    of that role. Its list scope is [`ListScope::Unified`].
//! 2. **Favorites**: the starred folders, flat, with their own counts.
//! 3. **One node per account** with its folder tree.
//!
//! Sections without children are left out. The UI decides which sections to show
//! (setting `folder_pane.modes`).

use mazemail_types::{AccountId, FolderId, FolderRole};
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

fn convert(node: &store::FolderNode) -> FolderNode {
    FolderNode::folder(&node.folder, node.children.iter().map(convert).collect())
}

/// Builds the folder pane from the store's accounts and folder trees.
pub(crate) fn build(
    accounts: &[Account],
    folders: &[Folder],
    trees: &[(AccountId, Vec<store::FolderNode>)],
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

    let favorites: Vec<FolderNode> = folders
        .iter()
        .filter(|f| f.favorite)
        .map(|f| FolderNode::folder(f, Vec::new()))
        .collect();
    if !favorites.is_empty() {
        out.push(FolderNode::header(
            FolderNodeKind::FavoritesSection,
            "Favorites",
            favorites,
        ));
    }

    for account in accounts {
        let children = trees
            .iter()
            .find(|(id, _)| *id == account.id)
            .map(|(_, tree)| tree.iter().map(convert).collect())
            .unwrap_or_default();
        out.push(FolderNode::header(
            FolderNodeKind::Account(account.id),
            &account.name,
            children,
        ));
    }
    out
}

//! Domain types shared by every Maze crate.
//!
//! This crate is layer L0: no IO, no async, no heavy dependencies.

mod address;
mod flags;
mod folder;
mod ids;
mod sensitive;

pub use address::{Address, AddressParseError};
pub use flags::Flags;
pub use folder::FolderRole;
pub use ids::{
    AccountId, BlobId, FolderId, IdentityId, MessageId, ParseIdError, PublicId, TagId, ThreadId,
};
pub use sensitive::Sensitive;

/// The product name, used wherever people read it (window titles, dialogs, docs).
pub const PRODUCT_NAME: &str = "Maze";

/// The technical name: crate, binary, CLI, data directories, env var prefix.
pub const TECHNICAL_NAME: &str = "mazemail";

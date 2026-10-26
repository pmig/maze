//! Domain types shared by every Maze crate.
//!
//! This crate is layer L0: no IO, no async, no heavy dependencies.

mod address;
pub mod compose;
mod flags;
mod folder;
mod ids;
mod sensitive;
mod tenant;

pub use address::{Address, AddressListError, AddressParseError};
pub use flags::Flags;
pub use folder::FolderRole;
pub use ids::{
    AccountId, BlobId, DraftId, FolderId, IdentityId, MessageId, ParseIdError, PublicId, TagId,
    ThreadId,
};
pub use sensitive::Sensitive;
pub use tenant::{InvalidTenant, MicrosoftTenant};

/// The product name, used wherever people read it (window titles, dialogs, docs).
pub const PRODUCT_NAME: &str = "Maze";

/// The technical name: crate, binary, CLI, data directories, env var prefix.
pub const TECHNICAL_NAME: &str = "mazemail";

//! Maze contact sources (ADR-024, PLAN-contacts §3). Layer L1; depends on `auth`
//! (same layer, for the account's OAuth tokens) and `mazemail_types`. No GPUI, no
//! store: `mazemail_core` stores what a sync returns.
//!
//! The server owns the contacts. Maze keeps a read-only copy and writes through: a
//! create or edit goes to the server first, and only the server's answer is stored.
//!
//! - [`ContactSource`]: list a book page by page with sync tokens, create, update
//!   (etag-checked, [`ContactError::Conflict`] carries the server's version) and
//!   photos. [`sync_book`] runs the paging loop and restarts with a full listing when
//!   a sync token expired.
//! - [`GoogleContacts`]: the Google People API (My Contacts, Other contacts),
//!   see [`google`].
//! - [`MicrosoftContacts`]: Microsoft Graph (the default Contacts folder of
//!   Outlook.com and Microsoft 365), see [`microsoft`].
//! - [`ContactFields`] / [`Person`]: the model, with [`vcard`] (vCard 4.0) for the
//!   `contact` table's `vcard` column.
//! - [`Book`]: an account's address book as named in the `contact` table
//!   (`google:<account uuid>`, `google-other:<account uuid>`,
//!   `microsoft:<account uuid>`); [`Provider`] says which books an account has.
//!
//! Contact data is never logged: the model's `Debug` impls only show counts.

mod error;
pub mod google;
pub mod microsoft;
mod model;
mod source;
pub mod vcard;

pub use error::ContactError;
pub use google::{Backoff, GoogleContacts, fetch_photo};
pub use microsoft::MicrosoftContacts;
pub use model::{
    Book, BookKind, ContactFields, Labeled, Name, NotASyncedBook, Organization, Person, Photo,
    PostalAddress, Provider, SourceRef, is_valid_mailbox, shared_book_mailbox, shared_book_name,
};
pub use source::{
    AccessTokens, ContactSource, PageUpdate, SharedContactTokens, SyncPage, SyncSummary, sync_book,
};

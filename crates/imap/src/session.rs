//! The [`ImapSession`] trait: everything the sync engine needs from an IMAP server.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use mazemail_types::Flags;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::gmail::LabelOp;
use crate::types::{
    AppendUid, Caps, ChangeSet, CopyUid, FlagChange, FlagOp, FolderStatus, HeaderFetch,
    IdleOutcome, QresyncArgs, RemoteFolder, SelectInfo, SessionEvent,
};
use crate::uid_set::UidSet;

/// One authenticated IMAP connection.
///
/// Implemented by [`ImapClient`](crate::ImapClient) (a real server) and, behind the
/// `fake` feature, by `FakeImap` (in memory). The sync engine is written against
/// this trait only.
///
/// # Design
///
/// - The trait uses `#[async_trait]`, so it works both as `Box<dyn ImapSession>` and
///   as a generic bound, and every returned future is `Send` (sessions move between
///   Tokio worker threads). Native `async fn` in traits is not dyn-compatible yet.
/// - Folder names are the decoded UTF-8 names from [`RemoteFolder::name`]; encoding
///   to modified UTF-7 happens inside.
/// - Commands that operate on messages (`fetch_*`, `store_*`, `move`, `copy`,
///   `uid_search_all`, `expunge_uids`, `idle`) need a selected folder.
/// - Unsolicited server data (`EXISTS`, `EXPUNGE`, `VANISHED`, flag `FETCH`es) that
///   arrives during any command or while idling is queued; read it with
///   [`take_events`](ImapSession::take_events).
/// - After an error that is not [`recoverable`](crate::ImapError::is_recoverable),
///   every further call fails with [`ImapError::Broken`](crate::ImapError::Broken):
///   drop the session and reconnect.
#[async_trait]
pub trait ImapSession: Send {
    /// The current capability list (refreshed after login and STARTTLS).
    async fn capabilities(&mut self) -> Result<Caps>;

    /// All folders, with SPECIAL-USE roles, delimiter, attributes and subscription state.
    async fn list(&mut self) -> Result<Vec<RemoteFolder>>;

    /// Select a folder read-write.
    ///
    /// With `qresync` and a server that supports QRESYNC, sends
    /// `SELECT f (QRESYNC (…))` and returns the vanished UIDs and changed flags since
    /// the given mod-sequence. Otherwise uses `(CONDSTORE)` when available so that
    /// `highest_modseq` is filled.
    async fn select(&mut self, folder: &str, qresync: Option<QresyncArgs>) -> Result<SelectInfo>;

    /// Flags of messages changed since `since_modseq` (`UID FETCH 1:* (FLAGS)
    /// (CHANGEDSINCE m)`), plus vanished UIDs when QRESYNC is enabled.
    ///
    /// Needs CONDSTORE; returns [`ImapError::Unsupported`](crate::ImapError::Unsupported) otherwise.
    async fn fetch_changes(&mut self, since_modseq: u64) -> Result<ChangeSet>;

    /// Flags (and mod-sequences, Gmail labels when available) of the given messages.
    /// The fallback for servers without CONDSTORE.
    async fn fetch_flags(&mut self, uids: &UidSet) -> Result<Vec<FlagChange>>;

    /// Header data for the message list: flags, size, internal date, raw header
    /// block, BODYSTRUCTURE summary, Gmail metadata. Sorted by UID ascending; UIDs
    /// outside `uids` (an `n:*` artifact) are dropped.
    async fn fetch_headers(&mut self, uids: &UidSet) -> Result<Vec<HeaderFetch>>;

    /// One body section without setting `\Seen` (`BODY.PEEK[section]`), e.g. `1.2`,
    /// `TEXT`, `HEADER`, `2.MIME`.
    ///
    /// The bytes are returned as stored, still in the part's
    /// `Content-Transfer-Encoding` (see [`BodyPart::encoding`](crate::BodyPart::encoding)).
    async fn fetch_part(&mut self, uid: u32, section: &str) -> Result<Bytes>;

    /// The complete raw message without setting `\Seen` (`BODY.PEEK[]`).
    async fn fetch_raw(&mut self, uid: u32) -> Result<Bytes>;

    /// Every UID in the selected folder (`UID SEARCH ALL`), for expunge detection.
    async fn uid_search_all(&mut self) -> Result<UidSet>;

    /// Change flags and keywords (`UID STORE … FLAGS.SILENT`).
    async fn store_flags(&mut self, uids: &UidSet, op: &FlagOp) -> Result<()>;

    /// Change Gmail labels (`UID STORE … X-GM-LABELS`). Gmail only.
    async fn store_gmail_labels(&mut self, uids: &UidSet, op: &LabelOp) -> Result<()>;

    /// Move messages (`UID MOVE`). Without MOVE: `UID COPY`, `+FLAGS (\Deleted)` and,
    /// with UIDPLUS, `UID EXPUNGE` (without UIDPLUS the source copies stay marked
    /// `\Deleted` so that other deleted messages are not expunged by accident).
    ///
    /// Returns `COPYUID` when the server reports it.
    async fn r#move(&mut self, uids: &UidSet, to: &str) -> Result<Option<CopyUid>>;

    /// Copy messages (`UID COPY`). Returns `COPYUID` when the server reports it.
    async fn copy(&mut self, uids: &UidSet, to: &str) -> Result<Option<CopyUid>>;

    /// Permanently remove messages that are marked `\Deleted` (`UID EXPUNGE`, UIDPLUS).
    async fn expunge_uids(&mut self, uids: &UidSet) -> Result<()>;

    /// Append a message with `flags` and the keywords (tags) in `keywords`. Returns
    /// `APPENDUID` when the server reports it.
    async fn append(
        &mut self,
        folder: &str,
        raw: &[u8],
        flags: Flags,
        keywords: &[String],
    ) -> Result<Option<AppendUid>>;

    /// Create a folder.
    async fn create(&mut self, folder: &str) -> Result<()>;

    /// Rename a folder.
    async fn rename(&mut self, from: &str, to: &str) -> Result<()>;

    /// Delete a folder.
    async fn delete(&mut self, folder: &str) -> Result<()>;

    /// Subscribe (`on = true`) or unsubscribe a folder.
    async fn subscribe(&mut self, folder: &str, on: bool) -> Result<()>;

    /// Cheap folder counters (`STATUS`): messages, unseen, UIDNEXT, UIDVALIDITY and
    /// HIGHESTMODSEQ (with CONDSTORE). Don't call it on the selected folder.
    async fn status(&mut self, folder: &str) -> Result<FolderStatus>;

    /// Wait for changes in the selected folder (`IDLE`) until the server reports new
    /// data, `timeout` elapses or `cancel` fires. Re-issuing IDLE periodically
    /// (every 25 minutes) is the caller's job.
    async fn idle(&mut self, cancel: CancellationToken, timeout: Duration) -> Result<IdleOutcome>;

    /// `NOOP`: lets the server deliver pending unsolicited data.
    async fn noop(&mut self) -> Result<()>;

    /// Drain the queued unsolicited events, oldest first.
    fn take_events(&mut self) -> Vec<SessionEvent>;

    /// End the session (`LOGOUT`). The session is unusable afterwards.
    async fn logout(&mut self) -> Result<()>;
}

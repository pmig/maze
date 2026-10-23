//! Typed reads and writes. Reads are on the [`Queries`] trait (implemented by
//! [`Reader`] and [`WriteTx`]); writes are methods of [`WriteTx`].

pub(crate) mod accounts;
pub(crate) mod agents;
pub(crate) mod folders;
pub(crate) mod lists;
pub(crate) mod messages;
pub(crate) mod misc;
pub(crate) mod ops;
pub(crate) mod threads;

use std::collections::BTreeSet;

use mazemail_types::{
    AccountId, Flags, FolderId, FolderRole, IdentityId, MessageId, TagId, ThreadId,
};
use rusqlite::Connection;

use crate::model::{
    Account, AgentThread, AgentThreadId, ApiToken, AuditEntry, AuditQuery, Contact, Folder,
    FolderNode, Identity, IdentityRule, ListFilter, ListKey, ListScope, MessageDetail,
    MessageScope, MetadataFilter, OutboxId, OutboxItem, Page, PendingOp, Proposal, ProposalId,
    ProposalState, Rule, SavedSearch, Signature, SignatureId, SmtpServer, SmtpServerId, SortOrder,
    Tag, ThreadRow, TokenId, UnixTime,
};
use crate::{Reader, Result, WriteTx};

mod sealed {
    /// Only this crate can implement [`Queries`](super::Queries) or call its
    /// connection accessor.
    pub trait Sealed {}

    /// Proof of being inside this crate.
    #[derive(Debug, Clone, Copy)]
    pub struct Token;
}

impl sealed::Sealed for Reader {}
impl sealed::Sealed for WriteTx<'_> {}

/// Read queries, available on a pooled [`Reader`] and inside a write job
/// ([`WriteTx`], where they see the job's own uncommitted changes).
pub trait Queries: sealed::Sealed {
    #[doc(hidden)]
    fn connection(&self, token: sealed::Token) -> &Connection;

    // ---- accounts and settings ----------------------------------------------------

    /// All accounts, by ordinal.
    fn accounts(&self) -> Result<Vec<Account>> {
        accounts::accounts(self.connection(sealed::Token))
    }

    /// One account.
    fn account(&self, id: AccountId) -> Result<Option<Account>> {
        accounts::account(self.connection(sealed::Token), id)
    }

    /// The account with this [`Account::uuid`].
    fn account_by_uuid(&self, uuid: uuid::Uuid) -> Result<Option<Account>> {
        accounts::account_by_uuid(self.connection(sealed::Token), uuid)
    }

    /// Identities of one account, or of all accounts.
    fn identities(&self, account: Option<AccountId>) -> Result<Vec<Identity>> {
        accounts::identities(self.connection(sealed::Token), account)
    }

    /// One identity.
    fn identity(&self, id: IdentityId) -> Result<Option<Identity>> {
        accounts::identity(self.connection(sealed::Token), id)
    }

    /// The rules that select an identity, by ordinal.
    fn identity_rules(&self, identity: IdentityId) -> Result<Vec<IdentityRule>> {
        accounts::identity_rules(self.connection(sealed::Token), identity)
    }

    /// All SMTP servers.
    fn smtp_servers(&self) -> Result<Vec<SmtpServer>> {
        accounts::smtp_servers(self.connection(sealed::Token))
    }

    /// One SMTP server.
    fn smtp_server(&self, id: SmtpServerId) -> Result<Option<SmtpServer>> {
        accounts::smtp_server(self.connection(sealed::Token), id)
    }

    /// All signatures.
    fn signatures(&self) -> Result<Vec<Signature>> {
        accounts::signatures(self.connection(sealed::Token))
    }

    /// One signature.
    fn signature(&self, id: SignatureId) -> Result<Option<Signature>> {
        accounts::signature(self.connection(sealed::Token), id)
    }

    /// All tags, by ordinal.
    fn tags(&self) -> Result<Vec<Tag>> {
        misc::tags(self.connection(sealed::Token))
    }

    /// The tag for an IMAP keyword (case-insensitive).
    fn tag_by_keyword(&self, keyword: &str) -> Result<Option<Tag>> {
        misc::tag_by_keyword(self.connection(sealed::Token), keyword)
    }

    /// Rules that apply to `account` (global ones included), or all rules.
    fn rules(&self, account: Option<AccountId>) -> Result<Vec<Rule>> {
        misc::rules(self.connection(sealed::Token), account)
    }

    /// All saved searches.
    fn saved_searches(&self) -> Result<Vec<SavedSearch>> {
        misc::saved_searches(self.connection(sealed::Token))
    }

    /// Address completion: contacts whose address or a name word starts with `prefix`.
    fn search_contacts(&self, prefix: &str, limit: usize) -> Result<Vec<Contact>> {
        misc::search_contacts(self.connection(sealed::Token), prefix, limit)
    }

    /// The contact for an address (case-insensitive), address book entries first.
    fn contact_by_email(&self, email: &str) -> Result<Option<Contact>> {
        misc::contact_by_email(self.connection(sealed::Token), email)
    }

    // ---- folders ------------------------------------------------------------------

    /// Folders of an account (or all), by ordinal and name.
    fn folders(&self, account: Option<AccountId>) -> Result<Vec<Folder>> {
        folders::folders(self.connection(sealed::Token), account)
    }

    /// The folder tree of an account (or of all accounts).
    fn folder_tree(&self, account: Option<AccountId>) -> Result<Vec<FolderNode>> {
        folders::folder_tree(self.connection(sealed::Token), account)
    }

    /// One folder.
    fn folder(&self, id: FolderId) -> Result<Option<Folder>> {
        folders::folder(self.connection(sealed::Token), id)
    }

    /// A folder by its server name.
    fn folder_by_remote_name(
        &self,
        account: AccountId,
        remote_name: &str,
    ) -> Result<Option<Folder>> {
        folders::folder_by_remote_name(self.connection(sealed::Token), account, remote_name)
    }

    /// Folders with a role, in one account or all (unified folders).
    fn folders_with_role(
        &self,
        role: FolderRole,
        account: Option<AccountId>,
    ) -> Result<Vec<Folder>> {
        folders::folders_with_role(self.connection(sealed::Token), role, account)
    }

    // ---- messages -----------------------------------------------------------------

    /// Everything about a message, with locations and tags.
    fn message(&self, id: MessageId) -> Result<Option<MessageDetail>> {
        messages::message(self.connection(sealed::Token), id)
    }

    /// The message at `uid` in `folder`.
    fn message_by_uid(&self, folder: FolderId, uid: u32) -> Result<Option<MessageId>> {
        messages::message_by_uid(self.connection(sealed::Token), folder, uid)
    }

    /// Messages of an account with this Message-ID (copies are separate messages on
    /// non-Gmail accounts).
    fn messages_by_rfc_id(
        &self,
        account: AccountId,
        rfc_message_id: &str,
    ) -> Result<Vec<MessageId>> {
        messages::messages_by_rfc_id(self.connection(sealed::Token), account, rfc_message_id)
    }

    /// The message with this Gmail `X-GM-MSGID`.
    fn message_by_gm_msgid(&self, account: AccountId, gm_msgid: u64) -> Result<Option<MessageId>> {
        messages::message_by_gm_msgid(self.connection(sealed::Token), account, gm_msgid)
    }

    /// UIDs and flags of a folder's locations, ascending (sync diffs).
    fn folder_uids(&self, folder: FolderId) -> Result<Vec<(u32, Flags)>> {
        messages::folder_uids(self.connection(sealed::Token), folder)
    }

    // ---- threads ------------------------------------------------------------------

    /// Threads containing a message with one of these Message-IDs, or one that refers
    /// to them (the lookup the JWZ threader needs).
    fn threads_for_rfc_ids(
        &self,
        account: AccountId,
        rfc_ids: &[&str],
    ) -> Result<BTreeSet<ThreadId>> {
        threads::threads_for_rfc_ids(self.connection(sealed::Token), account, rfc_ids)
    }

    /// The thread with this Gmail `X-GM-THRID`.
    fn thread_for_gm_thrid(&self, account: AccountId, gm_thrid: u64) -> Result<Option<ThreadId>> {
        threads::thread_for_gm_thrid(self.connection(sealed::Token), account, gm_thrid)
    }

    /// Messages of a thread (with a location), oldest first.
    fn thread_message_ids(&self, thread: ThreadId) -> Result<Vec<MessageId>> {
        threads::thread_message_ids(self.connection(sealed::Token), thread)
    }

    /// One page of thread list keys (see [`ListKey`], [`Page`]). Keys are ordered by
    /// `(date, thread id)`; pass the last key of a page as `Page::after` for the next.
    ///
    /// Unified scopes merge the per-folder keysets; a thread present in two folders of
    /// the same role can show up again on a later page, so snapshot builders should
    /// skip thread ids they already have.
    fn thread_list_keys(
        &self,
        scope: ListScope,
        sort: SortOrder,
        filter: &ListFilter,
        page: Page,
    ) -> Result<Vec<ListKey>> {
        lists::thread_list_keys(self.connection(sealed::Token), scope, sort, filter, page)
    }

    /// Row data for the visible threads, in the order given. With `context`, dates and
    /// counts are those in that folder.
    fn thread_rows(&self, ids: &[ThreadId], context: Option<FolderId>) -> Result<Vec<ThreadRow>> {
        threads::thread_rows(self.connection(sealed::Token), ids, context)
    }

    // ---- search support -----------------------------------------------------------

    /// The candidates (for example search hits) that match `filter`, in the order given.
    /// Detached messages never match.
    fn filter_message_ids(
        &self,
        candidates: &[MessageId],
        filter: &MetadataFilter,
    ) -> Result<Vec<MessageId>> {
        lists::filter_message_ids(self.connection(sealed::Token), candidates, filter)
    }

    /// Message ids in a narrow scope, ascending (an id filter applied before searching).
    fn message_ids_in_scope(&self, scope: &MessageScope) -> Result<Vec<MessageId>> {
        lists::message_ids_in_scope(self.connection(sealed::Token), scope)
    }

    /// How many of `ids` are in each folder (folder facet).
    fn folder_counts(&self, ids: &[MessageId]) -> Result<Vec<(FolderId, u64)>> {
        lists::folder_counts(self.connection(sealed::Token), ids)
    }

    /// How many of `ids` carry each tag (tag facet).
    fn tag_counts(&self, ids: &[MessageId]) -> Result<Vec<(TagId, u64)>> {
        lists::tag_counts(self.connection(sealed::Token), ids)
    }

    // ---- ops, outbox --------------------------------------------------------------

    /// An account's pending ops in replay order.
    fn pending_ops(&self, account: AccountId) -> Result<Vec<PendingOp>> {
        ops::pending_ops(self.connection(sealed::Token), account)
    }

    /// The next op to replay for an account, if it is due at `now`.
    fn next_pending_op(&self, account: AccountId, now: UnixTime) -> Result<Option<PendingOp>> {
        ops::next_pending_op(self.connection(sealed::Token), account, now)
    }

    /// Accounts that have queued ops.
    fn accounts_with_pending_ops(&self) -> Result<Vec<AccountId>> {
        ops::accounts_with_pending_ops(self.connection(sealed::Token))
    }

    /// The whole outbox.
    fn outbox(&self) -> Result<Vec<OutboxItem>> {
        ops::outbox(self.connection(sealed::Token))
    }

    /// One outbox item.
    fn outbox_item(&self, id: OutboxId) -> Result<Option<OutboxItem>> {
        ops::outbox_item(self.connection(sealed::Token), id)
    }

    /// Queued outbox items due at `now`.
    fn outbox_due(&self, now: UnixTime) -> Result<Vec<OutboxItem>> {
        ops::outbox_due(self.connection(sealed::Token), now)
    }

    // ---- tokens, proposals, audit, agents -----------------------------------------

    /// All API tokens (hashes only).
    fn api_tokens(&self) -> Result<Vec<ApiToken>> {
        agents::api_tokens(self.connection(sealed::Token))
    }

    /// The token for a presented secret (hashed before the lookup).
    fn api_token_by_secret(&self, secret: &str) -> Result<Option<ApiToken>> {
        agents::api_token_by_secret(self.connection(sealed::Token), secret)
    }

    /// The token with this stored hash (for callers that hash secrets themselves).
    fn api_token_by_hash(&self, hash: &[u8; 32]) -> Result<Option<ApiToken>> {
        agents::api_token_by_hash(self.connection(sealed::Token), hash)
    }

    /// One token by id.
    fn api_token(&self, id: TokenId) -> Result<Option<ApiToken>> {
        agents::api_token(self.connection(sealed::Token), id)
    }

    /// One proposal with its items.
    fn proposal(&self, id: ProposalId) -> Result<Option<Proposal>> {
        agents::proposal(self.connection(sealed::Token), id)
    }

    /// Proposals (optionally in one state), newest first.
    fn proposals(&self, state: Option<ProposalState>) -> Result<Vec<Proposal>> {
        agents::proposals(self.connection(sealed::Token), state)
    }

    /// Audit entries, newest first.
    fn audit_log(&self, query: &AuditQuery) -> Result<Vec<AuditEntry>> {
        agents::audit_log(self.connection(sealed::Token), query)
    }

    /// Agent conversations, most recently updated first.
    fn agent_threads(&self) -> Result<Vec<AgentThread>> {
        agents::agent_threads(self.connection(sealed::Token))
    }

    /// One agent conversation.
    fn agent_thread(&self, id: AgentThreadId) -> Result<Option<AgentThread>> {
        agents::agent_thread(self.connection(sealed::Token), id)
    }
}

impl Queries for Reader {
    fn connection(&self, _: sealed::Token) -> &Connection {
        self.conn()
    }
}

impl Queries for WriteTx<'_> {
    fn connection(&self, _: sealed::Token) -> &Connection {
        self.conn
    }
}

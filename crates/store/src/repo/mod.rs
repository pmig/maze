//! Typed reads and writes. Reads are on the [`Queries`] trait (implemented by
//! [`Reader`] and [`WriteTx`]); writes are methods of [`WriteTx`].

pub(crate) mod accounts;
pub(crate) mod agents;
pub(crate) mod contact_sync;
pub(crate) mod drafts;
pub(crate) mod folders;
pub(crate) mod lists;
pub(crate) mod messages;
pub(crate) mod misc;
pub(crate) mod ops;
pub(crate) mod remote_content;
pub(crate) mod secrets;
pub(crate) mod sort;
pub(crate) mod spelling;
pub(crate) mod threads;

use std::collections::BTreeSet;

use mazemail_types::{
    AccountId, DraftId, Flags, FolderId, FolderRole, IdentityId, MessageId, TagId, ThreadId,
};
use rusqlite::Connection;

use crate::model::{
    Account, AgentThread, AgentThreadId, ApiToken, AuditEntry, AuditQuery, Contact, Folder,
    FolderNode, Identity, IdentityRule, ListFilter, ListKey, ListOrder, ListScope, MessageDetail,
    MessageKey, MessagePage, MessageRow, MessageScope, MetadataFilter, OutboxId, OutboxItem,
    OutboxState, Page, PendingOp, Proposal, ProposalId, ProposalState, Rule, SavedSearch,
    Signature, SignatureId, SmtpServer, SmtpServerId, SortOrder, StoredDraft, Tag, ThreadRow,
    TokenId, TreeMessage, UnixTime,
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

    /// A synced address book's sync state (`google:<account uuid>`).
    fn contact_source(&self, book: &str) -> Result<Option<contact_sync::ContactSourceState>> {
        contact_sync::contact_source(self.connection(sealed::Token), book)
    }

    /// The synced address books of an account.
    fn contact_sources(&self, account: AccountId) -> Result<Vec<contact_sync::ContactSourceState>> {
        contact_sync::contact_sources(self.connection(sealed::Token), account)
    }

    /// A synced person by server id.
    fn server_contact(
        &self,
        book: &str,
        resource_name: &str,
    ) -> Result<Option<contact_sync::StoredServerContact>> {
        contact_sync::server_contact(self.connection(sealed::Token), book, resource_name)
    }

    /// The synced person whose `contact` row in `book` is `email`.
    fn server_contact_by_email(
        &self,
        book: &str,
        email: &str,
    ) -> Result<Option<contact_sync::StoredServerContact>> {
        contact_sync::server_contact_by_email(self.connection(sealed::Token), book, email)
    }

    /// The synced person a `contact` row belongs to.
    fn server_contact_of_entry(
        &self,
        entry: crate::model::ContactId,
    ) -> Result<Option<contact_sync::StoredServerContact>> {
        contact_sync::server_contact_of_entry(self.connection(sealed::Token), entry)
    }

    /// Every synced person of a book (also those without an address), by name.
    fn server_contacts(&self, book: &str) -> Result<Vec<contact_sync::StoredServerContact>> {
        contact_sync::server_contacts(self.connection(sealed::Token), book)
    }

    /// What Maze knows about a contact photo URL.
    fn contact_photo(&self, url: &str) -> Result<Option<contact_sync::ContactPhotoState>> {
        contact_sync::contact_photo(self.connection(sealed::Token), url)
    }

    /// The photo URL of the synced person with address `email` (in `book`, or any
    /// synced book, My Contacts first).
    fn contact_photo_url(&self, email: &str, book: Option<&str>) -> Result<Option<String>> {
        contact_sync::contact_photo_url(self.connection(sealed::Token), email, book)
    }

    /// Photo URLs of synced people never fetched, or fetched before `fetched_before`.
    fn contact_photos_due(&self, fetched_before: UnixTime, limit: usize) -> Result<Vec<String>> {
        contact_sync::contact_photos_due(self.connection(sealed::Token), fetched_before, limit)
    }

    /// Every address book entry, without vCards (the Address Book's lists).
    fn contact_list(&self) -> Result<Vec<Contact>> {
        misc::contact_list(self.connection(sealed::Token))
    }

    /// Every entry for an address (case-insensitive) in every book, with vCards.
    fn contacts_by_email(&self, email: &str) -> Result<Vec<Contact>> {
        misc::contacts_by_email(self.connection(sealed::Token), email)
    }

    /// Whether messages from `email` may load remote content without asking
    /// ("Always for this sender"; case-insensitive).
    fn remote_content_allowed(&self, email: &str) -> Result<bool> {
        remote_content::remote_content_allowed(self.connection(sealed::Token), email)
    }

    /// The personal spelling dictionary, sorted ("Add to Dictionary" in the composer).
    fn spelling_words(&self) -> Result<Vec<String>> {
        spelling::spelling_words(self.connection(sealed::Token))
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

    /// The messages (with a location) of these threads, ascending.
    fn messages_of_threads(&self, ids: &[ThreadId]) -> Result<Vec<MessageId>> {
        threads::messages_of_threads(self.connection(sealed::Token), ids)
    }

    /// The number of messages of each thread (in `context`, if the thread has messages
    /// there), in the order given; 0 for unknown threads. "Expand all" uses it.
    fn thread_message_counts(
        &self,
        ids: &[ThreadId],
        context: Option<FolderId>,
    ) -> Result<Vec<u32>> {
        threads::thread_message_counts(self.connection(sealed::Token), ids, context)
    }

    /// The messages of a thread (with a location; with `context`, only those in that
    /// folder), oldest first, with the headers its reply tree is built from.
    fn thread_tree_messages(
        &self,
        thread: ThreadId,
        context: Option<FolderId>,
    ) -> Result<Vec<TreeMessage>> {
        messages::thread_tree_messages(self.connection(sealed::Token), thread, context)
    }

    // ---- message lists (threads off) ------------------------------------------------

    /// One page of message list keys (see [`MessageKey`], [`MessagePage`]): the messages
    /// of a scope, ordered by `(date, message id)`; pass the last key of a page as
    /// `MessagePage::after` for the next. Filters apply per copy in folder scopes and to
    /// the effective flags in account scopes.
    ///
    /// Unified scopes merge the per-folder keysets; a message in two folders of the same
    /// role can show up again on a later page, so snapshot builders should skip message
    /// ids they already have.
    fn message_list_keys(
        &self,
        scope: ListScope,
        sort: SortOrder,
        filter: &ListFilter,
        page: MessagePage,
    ) -> Result<Vec<MessageKey>> {
        lists::message_list_keys(self.connection(sealed::Token), scope, sort, filter, page)
    }

    /// `ids` (the keys of a thread list, for example) in `order`; unknown threads are
    /// left out. `context` is the folder the list shows, `None` for unified and account
    /// lists and search results: like [`Queries::thread_rows`], dates and flags are then
    /// those of the whole thread. Each key carries the thread's date.
    ///
    /// A thread sorts by what its row shows: its newest message's date (in `context`);
    /// whether any of its messages is starred, unread or has attachments; its subject
    /// (the first message's); its newest message's sender, recipients, correspondents,
    /// size and received time; the names of the tags on any of its messages; its
    /// account's name. Subjects ignore reply/forward prefixes and case; empty text sorts
    /// last; ties go newest first, then by id. One `ORDER BY` over the ids joined by
    /// primary key: no index can serve these orders.
    fn sorted_thread_keys(
        &self,
        ids: &[ThreadId],
        context: Option<FolderId>,
        order: ListOrder,
    ) -> Result<Vec<ListKey>> {
        sort::sorted_thread_keys(self.connection(sealed::Token), ids, context, order)
    }

    /// `ids` (the keys of a message list) in `order`, like
    /// [`Queries::sorted_thread_keys`]; with `context` the flags are those of the copy
    /// in that folder.
    fn sorted_message_keys(
        &self,
        ids: &[MessageId],
        context: Option<FolderId>,
        order: ListOrder,
    ) -> Result<Vec<MessageKey>> {
        sort::sorted_message_keys(self.connection(sealed::Token), ids, context, order)
    }

    /// Row data for visible messages, in the order given. With `context`, the flags are
    /// those of the copy in that folder.
    fn message_rows(
        &self,
        ids: &[MessageId],
        context: Option<FolderId>,
    ) -> Result<Vec<MessageRow>> {
        messages::message_rows(self.connection(sealed::Token), ids, context)
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

    /// Outbox items in `state`, oldest first.
    fn outbox_in_state(&self, state: OutboxState) -> Result<Vec<OutboxItem>> {
        ops::outbox_in_state(self.connection(sealed::Token), state)
    }

    // ---- drafts -------------------------------------------------------------------

    /// One local draft.
    fn draft(&self, id: DraftId) -> Result<Option<StoredDraft>> {
        drafts::draft(self.connection(sealed::Token), id)
    }

    /// All local drafts, most recently edited first.
    fn drafts(&self) -> Result<Vec<StoredDraft>> {
        drafts::drafts(self.connection(sealed::Token))
    }

    // ---- account secrets ----------------------------------------------------------

    /// The `kind` secret of the account with this [`Account::uuid`] (ADR-017), wiped
    /// from memory when dropped. Never log it.
    fn account_secret(
        &self,
        account: uuid::Uuid,
        kind: &str,
    ) -> Result<Option<zeroize::Zeroizing<String>>> {
        secrets::account_secret(self.connection(sealed::Token), account, kind)
    }

    /// Accounts whose secrets may still be in the OS keychain: created before schema
    /// v5 and not moved into the store yet.
    fn legacy_secret_accounts(&self) -> Result<Vec<uuid::Uuid>> {
        secrets::legacy_secret_accounts(self.connection(sealed::Token))
    }

    /// The sign-in key of an agent (schema v6): the environment variable it is passed
    /// in and the secret, wiped from memory when dropped. Never log it.
    fn agent_secret(&self, agent_id: &str) -> Result<Option<(String, zeroize::Zeroizing<String>)>> {
        secrets::agent_secret(self.connection(sealed::Token), agent_id)
    }

    /// `(agent id, environment variable)` of every stored agent sign-in key, without
    /// the secrets.
    fn agent_secret_envs(&self) -> Result<Vec<(String, String)>> {
        secrets::agent_secret_envs(self.connection(sealed::Token))
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

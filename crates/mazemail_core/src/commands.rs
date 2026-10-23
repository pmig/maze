//! Mail commands: optimistic local apply, pending ops, audit and undo (PLAN §4.2).
//!
//! [`CoreHandle::apply`](crate::CoreHandle::apply) runs one [`MailCommand`] as **one
//! store write job**: it reads the current state, enqueues the server ops (see
//! [`crate::ops`]) *before* the optimistic local change, applies that change, and writes
//! the audit entry. All of it commits together or not at all, so a crash never leaves a
//! local change without its pending op (or the reverse). The store's change set then
//! reaches the UI as an event batch.
//!
//! # Semantics (Thunderbird's)
//!
//! - **Flags** are set on every location of a message (marking read writes all copies).
//! - **Move** moves each message's copy out of `from` (or out of every folder except
//!   Gmail's All Mail when `from` is `None`) into `to`, within one account.
//! - **Archive** moves to the identity's archive folder (the message's identity, else
//!   the account's default identity, else the account's Archive folder), into a year or
//!   year/month subfolder per the identity's granularity, creating it if needed. On
//!   Gmail it removes the `\Inbox` label.
//! - **Delete** moves to the account's Trash; on messages already in Trash, or with
//!   `permanent`, it deletes for good (not undoable). On Gmail every label is removed.
//! - **Folder commands** create, rename (children follow) and delete folders; special
//!   folders can't be renamed or deleted.
//!
//! # Undo
//!
//! The last undoable command with [`Origin::User`] can be undone with
//! [`CoreHandle::undo`](crate::CoreHandle::undo) (`cmd-z`): flags and tags are restored
//! per location, moves, archives and deletes put the copies back where they were. Undo
//! is a command of its own: it enqueues the reverse server ops and is audited.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mazemail_types::{AccountId, Flags, FolderId, FolderRole, MessageId, TagId, ThreadId};
use serde::{Deserialize, Serialize};
use store::{
    AccountKind, Folder, FolderPatch, Location, MessageDetail, MessageScope, MetadataFilter,
    NewFolder, NewLocation, OpId, ProposalId, Queries, RuleId, TokenId, WriteTx,
};
use time::{OffsetDateTime, UtcOffset};

use crate::ops::{
    ArchiveTarget, FlagTarget, LocationRef, OpPayload, edit_keywords, flag_target, location_ref,
};
use crate::sync_bridge::RefreshScope;

/// A change to mail, from any origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum MailCommand {
    /// Add and remove flags (`\Seen`, `\Flagged`, …).
    SetFlags {
        ids: Vec<MessageId>,
        add: Flags,
        remove: Flags,
    },
    /// Add and remove tags (IMAP keywords).
    SetTags {
        ids: Vec<MessageId>,
        add: Vec<TagId>,
        remove: Vec<TagId>,
    },
    /// Move into `to`. `from` names the folder the messages are moved out of (the
    /// folder shown); `None` moves every copy except Gmail's All Mail.
    Move {
        ids: Vec<MessageId>,
        to: FolderId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<FolderId>,
    },
    /// Copy into `to`.
    Copy { ids: Vec<MessageId>, to: FolderId },
    /// Archive per identity (folder and granularity); Gmail removes the Inbox label.
    Archive { ids: Vec<MessageId> },
    /// Move to Trash, or delete for good.
    Delete {
        ids: Vec<MessageId>,
        permanent: bool,
    },
    /// Create a folder.
    CreateFolder {
        account: AccountId,
        parent: Option<FolderId>,
        name: String,
    },
    /// Rename a folder.
    RenameFolder { folder: FolderId, name: String },
    /// Delete a folder and its subfolders.
    DeleteFolder { folder: FolderId },
    /// Mark every message in a folder read.
    MarkFolderRead { folder: FolderId },
    /// Synchronize now.
    Refresh { scope: RefreshScope },
}

impl MailCommand {
    /// A short name for audit entries and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::SetFlags { .. } => "set_flags",
            Self::SetTags { .. } => "set_tags",
            Self::Move { .. } => "move",
            Self::Copy { .. } => "copy",
            Self::Archive { .. } => "archive",
            Self::Delete { .. } => "delete",
            Self::CreateFolder { .. } => "create_folder",
            Self::RenameFolder { .. } => "rename_folder",
            Self::DeleteFolder { .. } => "delete_folder",
            Self::MarkFolderRead { .. } => "mark_folder_read",
            Self::Refresh { .. } => "refresh",
        }
    }
}

/// Who issued a command. Recorded in the audit log and the pending op.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// The person at the keyboard (UI).
    User,
    /// A filter rule.
    Rule { rule: RuleId },
    /// An agent, through a local API token (checked by the
    /// [`CommandAuthorizer`](crate::CommandAuthorizer)).
    Agent { token: TokenId },
    /// The `mazemail` CLI with a user token.
    Cli,
}

impl Origin {
    /// The text stored in `audit_log.origin` and `pending_op.origin`.
    pub fn as_text(&self) -> String {
        match self {
            Self::User => "user".to_owned(),
            Self::Rule { rule } => format!("rule:{rule}"),
            Self::Agent { token } => format!("agent:{token}"),
            Self::Cli => "cli".to_owned(),
        }
    }
}

/// What happened to a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Applied locally; server ops are queued.
    Applied,
    /// Stored as a proposal for review; nothing changed.
    Proposed(ProposalId),
    /// Passed on to the sync engine (refresh).
    Requested,
}

/// The result of [`CoreHandle::apply`](crate::CoreHandle::apply).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReceipt {
    /// Sequence number of the command in this session.
    pub seq: u64,
    /// What happened.
    pub outcome: CommandOutcome,
    /// Messages changed.
    pub messages: Vec<MessageId>,
    /// Pending ops queued.
    pub ops: Vec<OpId>,
    /// The folder a [`MailCommand::CreateFolder`] created.
    pub created_folder: Option<FolderId>,
    /// Whether [`CoreHandle::undo`](crate::CoreHandle::undo) can revert it now.
    pub undoable: bool,
}

/// What undo restores.
#[derive(Debug, Clone)]
pub(crate) enum UndoAction {
    /// Per-location flags before the command.
    Flags(Vec<(MessageId, Vec<(FolderId, Flags)>)>),
    /// Keywords to re-add and to remove again, per message.
    Keywords(Vec<(MessageId, Vec<String>, Vec<String>)>),
    /// Locations before the command.
    Locations(Vec<(MessageId, Vec<Location>)>),
    /// Copies to remove.
    RemoveCopies {
        folder: FolderId,
        messages: Vec<MessageId>,
    },
}

impl UndoAction {
    fn is_empty(&self) -> bool {
        match self {
            Self::Flags(items) => items.is_empty(),
            Self::Keywords(items) => items.is_empty(),
            Self::Locations(items) => items.is_empty(),
            Self::RemoveCopies { messages, .. } => messages.is_empty(),
        }
    }
}

/// Session context for executing commands.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecContext {
    /// For archive year/month folders.
    pub(crate) utc_offset: UtcOffset,
}

/// What a command did inside its transaction.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    pub(crate) messages: Vec<MessageId>,
    pub(crate) threads: BTreeSet<ThreadId>,
    pub(crate) ops: Vec<OpId>,
    /// The queued ops with their accounts, in queue order.
    pub(crate) queued: Vec<(AccountId, OpId)>,
    pub(crate) accounts: BTreeSet<AccountId>,
    pub(crate) created_folder: Option<FolderId>,
    pub(crate) undo: Option<UndoAction>,
}

/// Runs `command` inside a write job. `Refresh` must be handled by the caller.
pub(crate) fn apply_in_tx(
    w: &mut WriteTx<'_>,
    command: &MailCommand,
    origin: &str,
    ctx: ExecContext,
) -> store::Result<Applied> {
    let mut tx = Tx::new(w, origin, ctx);
    match command {
        MailCommand::SetFlags { ids, add, remove } => tx.set_flags(ids, *add, *remove)?,
        MailCommand::SetTags { ids, add, remove } => tx.set_tags(ids, add, remove)?,
        MailCommand::Move { ids, to, from } => tx.move_messages(ids, *to, *from)?,
        MailCommand::Copy { ids, to } => tx.copy_messages(ids, *to)?,
        MailCommand::Archive { ids } => tx.archive(ids)?,
        MailCommand::Delete { ids, permanent } => tx.delete(ids, *permanent)?,
        MailCommand::CreateFolder {
            account,
            parent,
            name,
        } => tx.create_folder(*account, *parent, name)?,
        MailCommand::RenameFolder { folder, name } => tx.rename_folder(*folder, name)?,
        MailCommand::DeleteFolder { folder } => tx.delete_folder(*folder)?,
        MailCommand::MarkFolderRead { folder } => tx.mark_folder_read(*folder)?,
        MailCommand::Refresh { .. } => {}
    }
    tx.audit(command.name(), command)?;
    Ok(tx.finish())
}

/// Reverts `action` inside a write job.
pub(crate) fn undo_in_tx(
    w: &mut WriteTx<'_>,
    action: &UndoAction,
    origin: &str,
    ctx: ExecContext,
) -> store::Result<Applied> {
    let mut tx = Tx::new(w, origin, ctx);
    match action {
        UndoAction::Flags(items) => tx.restore_flags(items)?,
        UndoAction::Keywords(items) => tx.restore_keywords(items)?,
        UndoAction::Locations(items) => tx.restore_locations(items)?,
        UndoAction::RemoveCopies { folder, messages } => tx.remove_copies(*folder, messages)?,
    }
    let detail = serde_json::json!({ "messages": tx.applied.messages.len() }).to_string();
    tx.w.audit(origin, "undo", Some(&detail))?;
    let mut applied = tx.finish();
    applied.undo = None;
    Ok(applied)
}

fn invalid(reason: &'static str) -> store::Error {
    store::Error::Invalid(reason)
}

fn not_found(entity: &'static str, id: impl std::fmt::Display) -> store::Error {
    store::Error::NotFound {
        entity,
        id: id.to_string(),
    }
}

struct Tx<'a, 'w> {
    w: &'a mut WriteTx<'w>,
    origin: &'a str,
    ctx: ExecContext,
    folders: HashMap<FolderId, Folder>,
    applied: Applied,
    touched: BTreeSet<MessageId>,
}

impl<'a, 'w> Tx<'a, 'w> {
    fn new(w: &'a mut WriteTx<'w>, origin: &'a str, ctx: ExecContext) -> Self {
        Self {
            w,
            origin,
            ctx,
            folders: HashMap::new(),
            applied: Applied::default(),
            touched: BTreeSet::new(),
        }
    }

    fn finish(mut self) -> Applied {
        self.applied.messages = self.touched.into_iter().collect();
        if self.applied.undo.as_ref().is_some_and(UndoAction::is_empty) {
            self.applied.undo = None;
        }
        self.applied
    }

    fn folder(&mut self, id: FolderId) -> store::Result<Folder> {
        if let Some(folder) = self.folders.get(&id) {
            return Ok(folder.clone());
        }
        let folder = self.w.folder(id)?.ok_or_else(|| not_found("folder", id))?;
        self.folders.insert(id, folder.clone());
        Ok(folder)
    }

    fn detail(&mut self, id: MessageId) -> store::Result<MessageDetail> {
        self.w.message(id)?.ok_or_else(|| not_found("message", id))
    }

    fn details(&mut self, ids: &[MessageId]) -> store::Result<Vec<MessageDetail>> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if seen.insert(*id) {
                out.push(self.detail(*id)?);
            }
        }
        Ok(out)
    }

    fn touch(&mut self, detail: &MessageDetail) {
        self.touched.insert(detail.id);
        self.applied.threads.insert(detail.thread);
    }

    fn enqueue(&mut self, account: AccountId, payload: &OpPayload) -> store::Result<()> {
        let id = payload.enqueue(self.w, account, self.origin)?;
        self.applied.ops.push(id);
        self.applied.queued.push((account, id));
        self.applied.accounts.insert(account);
        Ok(())
    }

    /// The full server name of a folder (its display name for local folders).
    fn remote_name(folder: &Folder) -> String {
        folder
            .remote_name
            .clone()
            .unwrap_or_else(|| folder.display_name.clone())
    }

    fn account_kind(&mut self, account: AccountId) -> store::Result<AccountKind> {
        Ok(self
            .w
            .account(account)?
            .ok_or_else(|| not_found("account", account))?
            .kind)
    }

    fn delimiter(&mut self, account: AccountId) -> store::Result<String> {
        let account = self
            .w
            .account(account)?
            .ok_or_else(|| not_found("account", account))?;
        Ok(
            serde_json::from_str::<serde_json::Value>(&account.config_json)
                .ok()
                .and_then(|config| config.get("delimiter")?.as_str().map(str::to_owned))
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| "/".to_owned()),
        )
    }

    fn role_folder(
        &mut self,
        account: AccountId,
        role: FolderRole,
    ) -> store::Result<Option<Folder>> {
        Ok(self
            .w
            .folders_with_role(role, Some(account))?
            .into_iter()
            .next())
    }

    fn audit(&mut self, action: &str, command: &MailCommand) -> store::Result<()> {
        const LISTED: usize = 100;
        let ids: Vec<String> = self
            .touched
            .iter()
            .take(LISTED)
            .map(ToString::to_string)
            .collect();
        let mut detail = serde_json::json!({
            "messages": self.touched.len(),
            "ids": ids,
            "ops": self.applied.ops.len(),
        });
        match command {
            MailCommand::Move { to, .. } | MailCommand::Copy { to, .. } => {
                detail["to"] = to.to_string().into();
            }
            MailCommand::SetFlags { add, remove, .. } => {
                detail["add"] = add.to_imap().into();
                detail["remove"] = remove.to_imap().into();
            }
            MailCommand::SetTags { add, remove, .. } => {
                detail["add"] = add
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .into();
                detail["remove"] = remove
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .into();
            }
            MailCommand::Delete { permanent, .. } => detail["permanent"] = (*permanent).into(),
            MailCommand::CreateFolder { .. } => {
                if let Some(folder) = self.applied.created_folder {
                    detail["folder"] = folder.to_string().into();
                }
            }
            MailCommand::RenameFolder { folder, .. }
            | MailCommand::DeleteFolder { folder }
            | MailCommand::MarkFolderRead { folder } => {
                detail["folder"] = folder.to_string().into();
            }
            MailCommand::Archive { .. } | MailCommand::Refresh { .. } => {}
        }
        self.w
            .audit(self.origin, action, Some(&detail.to_string()))?;
        Ok(())
    }

    // ---- flags and tags ---------------------------------------------------------

    fn set_flags(&mut self, ids: &[MessageId], add: Flags, remove: Flags) -> store::Result<()> {
        let details = self.details(ids)?;
        let mut by_account: BTreeMap<AccountId, Vec<FlagTarget>> = BTreeMap::new();
        let mut before = Vec::with_capacity(details.len());
        let mut changed = Vec::new();
        for detail in &details {
            let needs_change = detail
                .locations
                .iter()
                .any(|l| (l.flags | add) & !remove != l.flags);
            if !needs_change {
                continue;
            }
            for location in &detail.locations {
                let flags = (location.flags | add) & !remove;
                let target = flag_target(detail, location, flags, location.keywords.clone());
                by_account.entry(detail.account).or_default().push(target);
            }
            before.push((
                detail.id,
                detail
                    .locations
                    .iter()
                    .map(|l| (l.folder, l.flags))
                    .collect(),
            ));
            changed.push(detail.id);
            self.touch(detail);
        }
        for (account, targets) in by_account {
            self.enqueue(account, &OpPayload::set_flags(targets))?;
        }
        if !changed.is_empty() {
            self.w.set_flags(&changed, add, remove)?;
        }
        self.applied.undo = Some(UndoAction::Flags(before));
        Ok(())
    }

    fn set_tags(
        &mut self,
        ids: &[MessageId],
        add: &[TagId],
        remove: &[TagId],
    ) -> store::Result<()> {
        let tags = self.w.tags()?;
        let keyword = |id: &TagId| -> store::Result<String> {
            tags.iter()
                .find(|t| t.id == *id)
                .map(|t| t.keyword.clone())
                .ok_or_else(|| not_found("tag", id))
        };
        let add: Vec<String> = add.iter().map(keyword).collect::<store::Result<_>>()?;
        let remove: Vec<String> = remove.iter().map(keyword).collect::<store::Result<_>>()?;
        let details = self.details(ids)?;
        let mut by_account: BTreeMap<AccountId, Vec<FlagTarget>> = BTreeMap::new();
        let mut undo = Vec::new();
        let mut changed = Vec::new();
        for detail in &details {
            let present: BTreeSet<&str> = detail
                .locations
                .iter()
                .flat_map(|l| l.keywords.iter().map(String::as_str))
                .collect();
            let added: Vec<String> = add
                .iter()
                .filter(|k| !present.contains(k.as_str()))
                .cloned()
                .collect();
            let removed: Vec<String> = remove
                .iter()
                .filter(|k| present.contains(k.as_str()))
                .cloned()
                .collect();
            if added.is_empty() && removed.is_empty() {
                continue;
            }
            for location in &detail.locations {
                let keywords = edit_keywords(&location.keywords, &add, &remove);
                let target = flag_target(detail, location, location.flags, keywords);
                by_account.entry(detail.account).or_default().push(target);
            }
            // Undo re-adds what was removed and removes what was added.
            undo.push((detail.id, removed, added));
            changed.push(detail.id);
            self.touch(detail);
        }
        for (account, targets) in by_account {
            self.enqueue(account, &OpPayload::set_flags(targets))?;
        }
        if !changed.is_empty() {
            self.w.set_keywords(&changed, &add, &remove)?;
        }
        self.applied.undo = Some(UndoAction::Keywords(undo));
        Ok(())
    }

    fn restore_flags(
        &mut self,
        items: &[(MessageId, Vec<(FolderId, Flags)>)],
    ) -> store::Result<()> {
        let mut by_account: BTreeMap<AccountId, Vec<FlagTarget>> = BTreeMap::new();
        for (id, before) in items {
            let Some(detail) = self.w.message(*id)? else {
                continue;
            };
            for location in &detail.locations {
                let Some((_, flags)) = before.iter().find(|(f, _)| *f == location.folder) else {
                    continue;
                };
                if *flags == location.flags {
                    continue;
                }
                let target = flag_target(&detail, location, *flags, location.keywords.clone());
                by_account.entry(detail.account).or_default().push(target);
                self.w
                    .set_location_flags(detail.id, location.folder, *flags)?;
            }
            self.touch(&detail);
        }
        for (account, targets) in by_account {
            self.enqueue(account, &OpPayload::set_flags(targets))?;
        }
        Ok(())
    }

    fn restore_keywords(
        &mut self,
        items: &[(MessageId, Vec<String>, Vec<String>)],
    ) -> store::Result<()> {
        for (id, add, remove) in items {
            let Some(detail) = self.w.message(*id)? else {
                continue;
            };
            let targets = detail
                .locations
                .iter()
                .map(|location| {
                    let keywords = edit_keywords(&location.keywords, add, remove);
                    flag_target(&detail, location, location.flags, keywords)
                })
                .collect();
            self.enqueue(detail.account, &OpPayload::set_flags(targets))?;
            self.w.set_keywords(&[detail.id], add, remove)?;
            self.touch(&detail);
        }
        Ok(())
    }

    // ---- moving ---------------------------------------------------------------

    /// Moves `sources` of a message into `to` locally (the first source becomes the
    /// new location; the others are removed).
    fn relocate(
        &mut self,
        detail: &MessageDetail,
        sources: &[Location],
        to: FolderId,
    ) -> store::Result<()> {
        let mut in_target = detail.locations.iter().any(|l| l.folder == to);
        for source in sources {
            if in_target {
                self.w.remove_location(detail.id, source.folder)?;
            } else {
                self.w.move_location(detail.id, source.folder, to, None)?;
                in_target = true;
            }
        }
        self.touch(detail);
        Ok(())
    }

    /// The copies a move takes out: `from`, or everything but All Mail and `to`.
    fn move_sources(
        &mut self,
        detail: &MessageDetail,
        to: FolderId,
        from: Option<FolderId>,
    ) -> store::Result<Vec<Location>> {
        let mut sources = Vec::new();
        for location in &detail.locations {
            if location.folder == to {
                continue;
            }
            let take = match from {
                Some(from) => location.folder == from,
                None => self.folder(location.folder)?.role != Some(FolderRole::All),
            };
            if take {
                sources.push(location.clone());
            }
        }
        Ok(sources)
    }

    fn move_messages(
        &mut self,
        ids: &[MessageId],
        to: FolderId,
        from: Option<FolderId>,
    ) -> store::Result<()> {
        let target = self.folder(to)?;
        let details = self.details(ids)?;
        let mut undo = Vec::new();
        let mut planned = Vec::new();
        let mut items = Vec::new();
        for detail in details {
            if detail.account != target.account {
                return Err(invalid("moving messages between accounts is not supported"));
            }
            let sources = self.move_sources(&detail, to, from)?;
            if sources.is_empty() {
                continue;
            }
            items.extend(sources.iter().map(|source| location_ref(&detail, source)));
            planned.push((detail, sources));
        }
        if !items.is_empty() {
            self.enqueue(target.account, &OpPayload::move_to(items, target.id))?;
        }
        for (detail, sources) in planned {
            undo.push((detail.id, detail.locations.clone()));
            self.relocate(&detail, &sources, to)?;
        }
        self.applied.undo = Some(UndoAction::Locations(undo));
        Ok(())
    }

    fn copy_messages(&mut self, ids: &[MessageId], to: FolderId) -> store::Result<()> {
        let target = self.folder(to)?;
        let details = self.details(ids)?;
        let mut items = Vec::new();
        let mut planned = Vec::new();
        for detail in details {
            if detail.account != target.account {
                return Err(invalid(
                    "copying messages between accounts is not supported",
                ));
            }
            if detail.locations.iter().any(|l| l.folder == to) {
                continue;
            }
            let Some(source) = detail.locations.first() else {
                continue;
            };
            items.push(location_ref(&detail, source));
            planned.push(detail);
        }
        if !items.is_empty() {
            self.enqueue(target.account, &OpPayload::copy_to(items, target.id))?;
        }
        let mut copied = Vec::new();
        for detail in planned {
            let mut keywords: Vec<String> = detail
                .locations
                .iter()
                .flat_map(|l| l.keywords.iter().cloned())
                .collect();
            keywords.sort();
            keywords.dedup();
            self.w.add_location(
                detail.id,
                &NewLocation {
                    folder: to,
                    uid: None,
                    modseq: None,
                    flags: detail.effective_flags(),
                    keywords,
                },
            )?;
            copied.push(detail.id);
            self.touch(&detail);
        }
        self.applied.undo = Some(UndoAction::RemoveCopies {
            folder: to,
            messages: copied,
        });
        Ok(())
    }

    fn remove_copies(&mut self, folder: FolderId, messages: &[MessageId]) -> store::Result<()> {
        let folder_row = self.folder(folder)?;
        let mut items = Vec::new();
        let mut planned = Vec::new();
        for id in messages {
            let Some(detail) = self.w.message(*id)? else {
                continue;
            };
            let Some(location) = detail.locations.iter().find(|l| l.folder == folder) else {
                continue;
            };
            items.push(location_ref(&detail, location));
            planned.push(detail);
        }
        if !items.is_empty() {
            self.enqueue(folder_row.account, &OpPayload::delete_permanently(items))?;
        }
        for detail in planned {
            self.w.remove_location(detail.id, folder)?;
            self.touch(&detail);
        }
        Ok(())
    }

    fn restore_locations(&mut self, items: &[(MessageId, Vec<Location>)]) -> store::Result<()> {
        for (id, before) in items {
            let Some(detail) = self.w.message(*id)? else {
                continue;
            };
            let now_only: Vec<Location> = detail
                .locations
                .iter()
                .filter(|l| !before.iter().any(|b| b.folder == l.folder))
                .cloned()
                .collect();
            let before_only: Vec<Location> = before
                .iter()
                .filter(|b| !detail.locations.iter().any(|l| l.folder == b.folder))
                .cloned()
                .collect();
            if now_only.is_empty() && before_only.is_empty() {
                continue;
            }
            // Server side: move the new copies back into the first original folder and
            // copy into any further ones (Gmail labels).
            let mut back = before_only.iter();
            if let Some(first) = back.next() {
                if now_only.is_empty() {
                    if let Some(source) = detail.locations.first() {
                        let items = vec![location_ref(&detail, source)];
                        self.enqueue(detail.account, &OpPayload::copy_to(items, first.folder))?;
                    }
                } else {
                    let items: Vec<LocationRef> = now_only
                        .iter()
                        .map(|location| location_ref(&detail, location))
                        .collect();
                    self.enqueue(detail.account, &OpPayload::move_to(items, first.folder))?;
                }
                for extra in back {
                    // The first original folder gets its copy back (without a UID yet)
                    // from the op above; the further ones are copied from it.
                    let items = vec![LocationRef::new(detail.id, first.folder, None)];
                    self.enqueue(detail.account, &OpPayload::copy_to(items, extra.folder))?;
                }
            } else {
                let items: Vec<LocationRef> = now_only
                    .iter()
                    .map(|location| location_ref(&detail, location))
                    .collect();
                self.enqueue(detail.account, &OpPayload::delete_permanently(items))?;
            }
            // Local side: put the original copies back first, so the message is never
            // detached in between.
            for location in &before_only {
                self.w.add_location(
                    detail.id,
                    &NewLocation {
                        folder: location.folder,
                        uid: None,
                        modseq: None,
                        flags: location.flags,
                        keywords: location.keywords.clone(),
                    },
                )?;
            }
            for location in &now_only {
                self.w.remove_location(detail.id, location.folder)?;
            }
            self.touch(&detail);
        }
        Ok(())
    }

    // ---- archive and delete ------------------------------------------------------

    /// The archive folder for a message: its identity's, the account default
    /// identity's, or the account's Archive folder, plus the granularity.
    fn archive_base(&mut self, detail: &MessageDetail) -> store::Result<(Folder, i64)> {
        let identity = match detail.identity_hint {
            Some(id) => self.w.identity(id)?,
            None => None,
        };
        let identity = match identity {
            Some(identity) => Some(identity),
            None => {
                let identities = self.w.identities(Some(detail.account))?;
                identities
                    .iter()
                    .find(|i| i.input.is_default)
                    .or_else(|| identities.first())
                    .cloned()
            }
        };
        let granularity = identity.as_ref().map_or(1, |i| i.input.archive_granularity);
        let folder = match identity.and_then(|i| i.input.archive_folder) {
            Some(folder) => Some(self.folder(folder)?),
            None => self.role_folder(detail.account, FolderRole::Archive)?,
        };
        let folder = folder.ok_or_else(|| invalid("the account has no archive folder"))?;
        Ok((folder, granularity))
    }

    /// The subfolder `name` of `parent`, created (locally and as a pending op) if it
    /// doesn't exist.
    fn ensure_child(&mut self, parent: &Folder, name: &str) -> store::Result<Folder> {
        let existing = self
            .w
            .folders(Some(parent.account))?
            .into_iter()
            .find(|f| f.parent == Some(parent.id) && f.display_name == name);
        if let Some(folder) = existing {
            return Ok(folder);
        }
        let delimiter = self.delimiter(parent.account)?;
        let remote_name = match &parent.remote_name {
            Some(remote) => format!("{remote}{delimiter}{name}"),
            None => name.to_owned(),
        };
        let id = self.w.create_folder(&NewFolder {
            account: parent.account,
            parent: Some(parent.id),
            remote_name: Some(remote_name),
            display_name: name.to_owned(),
            role: None,
            is_label: false,
            sync_policy: None,
            ordinal: 0,
        })?;
        let folder = self.folder(id)?;
        self.enqueue(
            parent.account,
            &OpPayload::CreateFolder {
                name: Self::remote_name(&folder),
                folder: Some(folder.id),
                subscribe: true,
            },
        )?;
        Ok(folder)
    }

    fn archive_folder_for(&mut self, detail: &MessageDetail) -> store::Result<Folder> {
        let (base, granularity) = self.archive_base(detail)?;
        if granularity <= 0 {
            return Ok(base);
        }
        let date = OffsetDateTime::from_unix_timestamp(detail.date)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
            .to_offset(self.ctx.utc_offset);
        let year = self.ensure_child(&base, &date.year().to_string())?;
        if granularity == 1 {
            return Ok(year);
        }
        let month = format!("{}-{:02}", date.year(), u8::from(date.month()));
        self.ensure_child(&year, &month)
    }

    fn archive(&mut self, ids: &[MessageId]) -> store::Result<()> {
        let details = self.details(ids)?;
        let mut undo = Vec::new();
        let mut groups: BTreeMap<(AccountId, Option<FolderId>), Vec<LocationRef>> = BTreeMap::new();
        let mut planned: Vec<(MessageDetail, Vec<Location>, Option<FolderId>)> = Vec::new();
        for detail in details {
            if self.account_kind(detail.account)? == AccountKind::Gmail {
                let Some(inbox) = self.role_folder(detail.account, FolderRole::Inbox)? else {
                    continue;
                };
                let Some(location) = detail
                    .locations
                    .iter()
                    .find(|l| l.folder == inbox.id)
                    .cloned()
                else {
                    continue;
                };
                groups
                    .entry((detail.account, None))
                    .or_default()
                    .push(location_ref(&detail, &location));
                planned.push((detail, vec![location], None));
                continue;
            }
            let folder = self.archive_folder_for(&detail)?;
            let sources: Vec<Location> = detail
                .locations
                .iter()
                .filter(|l| l.folder != folder.id)
                .cloned()
                .collect();
            if sources.is_empty() {
                continue;
            }
            groups
                .entry((detail.account, Some(folder.id)))
                .or_default()
                .extend(sources.iter().map(|source| location_ref(&detail, source)));
            planned.push((detail, sources, Some(folder.id)));
        }
        for ((account, folder), items) in groups {
            let target = match folder {
                Some(folder) => ArchiveTarget::Folder(folder),
                None => ArchiveTarget::GmailRemoveInbox,
            };
            self.enqueue(account, &OpPayload::archive(items, target))?;
        }
        for (detail, sources, to) in planned {
            undo.push((detail.id, detail.locations.clone()));
            match to {
                Some(to) => self.relocate(&detail, &sources, to)?,
                None => {
                    for source in &sources {
                        self.w.remove_location(detail.id, source.folder)?;
                    }
                    self.touch(&detail);
                }
            }
        }
        self.applied.undo = Some(UndoAction::Locations(undo));
        Ok(())
    }

    fn delete(&mut self, ids: &[MessageId], permanent: bool) -> store::Result<()> {
        let details = self.details(ids)?;
        let mut undo = Vec::new();
        let mut trash_groups: BTreeMap<(AccountId, FolderId), Vec<LocationRef>> = BTreeMap::new();
        let mut purge_groups: BTreeMap<AccountId, Vec<LocationRef>> = BTreeMap::new();
        let mut to_trash: Vec<(MessageDetail, Vec<Location>, FolderId)> = Vec::new();
        let mut to_purge: Vec<MessageDetail> = Vec::new();
        for detail in details {
            let trash = if permanent {
                None
            } else {
                self.role_folder(detail.account, FolderRole::Trash)?
            };
            let sources: Vec<Location> = match &trash {
                Some(trash) => detail
                    .locations
                    .iter()
                    .filter(|l| l.folder != trash.id)
                    .cloned()
                    .collect(),
                None => Vec::new(),
            };
            match trash {
                Some(trash) if !sources.is_empty() => {
                    // Every copy outside Trash moves there; on Gmail that removes every
                    // label (All Mail included), as `UID MOVE` to `[Gmail]/Trash` does.
                    trash_groups
                        .entry((detail.account, trash.id))
                        .or_default()
                        .extend(sources.iter().map(|source| location_ref(&detail, source)));
                    to_trash.push((detail, sources, trash.id));
                }
                _ => {
                    purge_groups.entry(detail.account).or_default().extend(
                        detail
                            .locations
                            .iter()
                            .map(|location| location_ref(&detail, location)),
                    );
                    to_purge.push(detail);
                }
            }
        }
        for ((account, trash), items) in trash_groups {
            self.enqueue(account, &OpPayload::trash(items, Some(trash)))?;
        }
        for (account, items) in purge_groups {
            self.enqueue(account, &OpPayload::delete_permanently(items))?;
        }
        for (detail, sources, trash) in to_trash {
            undo.push((detail.id, detail.locations.clone()));
            self.relocate(&detail, &sources, trash)?;
        }
        for detail in to_purge {
            self.touch(&detail);
            self.w.delete_message(detail.id)?;
        }
        self.applied.undo = Some(UndoAction::Locations(undo));
        Ok(())
    }

    // ---- folders ----------------------------------------------------------------

    fn create_folder(
        &mut self,
        account: AccountId,
        parent: Option<FolderId>,
        name: &str,
    ) -> store::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("a folder needs a name"));
        }
        let delimiter = self.delimiter(account)?;
        if name.contains(delimiter.as_str()) {
            return Err(invalid(
                "a folder name can't contain the hierarchy delimiter",
            ));
        }
        let parent = match parent {
            Some(id) => {
                let parent = self.folder(id)?;
                if parent.account != account {
                    return Err(invalid("the parent folder belongs to another account"));
                }
                Some(parent)
            }
            None => None,
        };
        let remote_name = match parent.as_ref().and_then(|p| p.remote_name.as_ref()) {
            Some(remote) => format!("{remote}{delimiter}{name}"),
            None => name.to_owned(),
        };
        let is_label = self.account_kind(account)? == AccountKind::Gmail;
        let id = self.w.create_folder(&NewFolder {
            account,
            parent: parent.as_ref().map(|p| p.id),
            remote_name: Some(remote_name),
            display_name: name.to_owned(),
            role: None,
            is_label,
            sync_policy: None,
            ordinal: 0,
        })?;
        let folder = self.folder(id)?;
        self.enqueue(
            account,
            &OpPayload::CreateFolder {
                name: Self::remote_name(&folder),
                folder: Some(folder.id),
                subscribe: true,
            },
        )?;
        self.applied.created_folder = Some(id);
        Ok(())
    }

    fn rename_folder(&mut self, id: FolderId, name: &str) -> store::Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(invalid("a folder needs a name"));
        }
        let folder = self.folder(id)?;
        if folder.role.is_some() {
            return Err(invalid("special folders can't be renamed"));
        }
        let delimiter = self.delimiter(folder.account)?;
        if name.contains(delimiter.as_str()) {
            return Err(invalid(
                "a folder name can't contain the hierarchy delimiter",
            ));
        }
        let old_remote = folder.remote_name.clone();
        let new_remote =
            old_remote
                .as_ref()
                .map(|remote| match remote.rsplit_once(delimiter.as_str()) {
                    Some((prefix, _)) => format!("{prefix}{delimiter}{name}"),
                    None => name.to_owned(),
                });
        self.w.update_folder(
            id,
            &FolderPatch {
                display_name: Some(name.to_owned()),
                remote_name: Some(new_remote.clone()),
                ..FolderPatch::default()
            },
        )?;
        // The server renames the subtree; keep the children's server names in step.
        if let (Some(old), Some(new)) = (&old_remote, &new_remote) {
            let prefix = format!("{old}{delimiter}");
            for child in self.w.folders(Some(folder.account))? {
                if let Some(rest) = child
                    .remote_name
                    .as_deref()
                    .and_then(|r| r.strip_prefix(&prefix))
                {
                    let renamed = format!("{new}{delimiter}{rest}");
                    self.w.update_folder(
                        child.id,
                        &FolderPatch {
                            remote_name: Some(Some(renamed)),
                            ..FolderPatch::default()
                        },
                    )?;
                }
            }
        }
        self.folders.clear();
        let renamed = self.folder(id)?;
        self.enqueue(
            folder.account,
            &OpPayload::RenameFolder {
                folder: Some(renamed.id),
                from: Self::remote_name(&folder),
                to: Self::remote_name(&renamed),
            },
        )?;
        Ok(())
    }

    fn delete_folder(&mut self, id: FolderId) -> store::Result<()> {
        let folder = self.folder(id)?;
        if folder.role.is_some() {
            return Err(invalid("special folders can't be deleted"));
        }
        // The local folder is deleted in this transaction, so the op only names it.
        self.enqueue(
            folder.account,
            &OpPayload::DeleteFolder {
                folder: None,
                name: Self::remote_name(&folder),
            },
        )?;
        self.w.delete_folder(id)?;
        Ok(())
    }

    fn mark_folder_read(&mut self, folder: FolderId) -> store::Result<()> {
        self.folder(folder)?;
        let in_folder = self.w.message_ids_in_scope(&MessageScope::Folder(folder))?;
        let unread = self.w.filter_message_ids(
            &in_folder,
            &MetadataFilter {
                folders: Some(vec![folder]),
                flags_unset: Flags::SEEN,
                ..MetadataFilter::default()
            },
        )?;
        self.set_flags(&unread, Flags::SEEN, Flags::empty())
    }
}

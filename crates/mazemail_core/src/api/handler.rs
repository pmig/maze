//! `local_api`'s [`CommandHandler`]: the mail commands of the `mazemail` CLI over
//! [`CoreHandle`].
//!
//! By the time a command arrives the dispatcher has checked the token, the profile
//! matrix and any approval. This handler still enforces the request's
//! [`EffectiveScope`]: every message, thread, folder and account a command names or
//! returns must be in it, and anything outside it is reported as `not_found`, exactly
//! like an id that doesn't exist (so a token can't probe other accounts).
//!
//! State changes (`archive`, `move`, `tag`, `flag`, `read`) honor
//! [`RequestContext::mode`]: [`ExecutionMode::Propose`] stores a proposal for the
//! review UI and changes nothing; [`ExecutionMode::Execute`] runs
//! [`CoreHandle::apply`] with [`Origin::Agent`], where the
//! [`TokenAuthorizer`](super::TokenAuthorizer) checks the token's profile once more.
//!
//! Every request through the socket carries a token, so every change runs as
//! [`Origin::Agent`]; [`Origin::Cli`] is reserved for an in-process CLI that doesn't
//! go through the local API.
//!
//! `import` runs the `import` crate into a local account and `export` writes the
//! matches of a query as mbox or Maildir (both `ask`, so the user approved them).
//! Commands whose crates aren't part of this build yet (`draft new|edit`, `send`,
//! `rule add`, `attachment save`) fail with a typed "not available in this version"
//! error (`internal`, exit code 1).

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use local_api::registry::args::{
    ArchiveArgs, ContactsArgs, ExportArgs, FlagArgs, FoldersArgs, IdentitiesArgs, ImportArgs,
    MoveArgs, ProposeArgs, ProposedAction, ReadArgs, SearchArgs, ShowArgs, ShowFormat, TagArgs,
};
use local_api::registry::output::{
    AttachmentMeta as ApiAttachment, ContactInfo, ExportOutput, FolderInfo, IdentityInfo,
    ImportOutput, MessageMeta, MutationOutcome, MutationOutput, ProposalOutput, SearchHit,
    ShowOutput, TagInfo,
};
use local_api::{
    Command, CommandError, CommandHandler, CommandOutput, EffectiveScope, ErrorCode, ExecutionMode,
    RequestContext,
};
use mazemail_types::{AccountId, Flags, FolderId, MessageId, PublicId, Sensitive, TagId, ThreadId};
use serde_json::{Value, json};
use store::{
    Folder, ListScope, MessageDetail, NewProposalItem, ProposalId, Queries as _, Reader, TokenId,
};
use time::OffsetDateTime;

use super::tokens::{TokenCache, proposal_title};
use super::{export, import};
use crate::commands::{CommandOutcome, MailCommand, Origin};
use crate::global_search::{SearchOpts, SearchSort};
use crate::handle::CoreHandle;
use crate::message::Body;
use crate::reader_text::{self as render, RenderedMessage, TextKind};
use crate::{Error, Snippet};

/// Search pages fetched at most to fill one `search` result.
const MAX_SEARCH_PAGES: usize = 20;

/// Messages one `export` writes at most.
const MAX_EXPORT: usize = 100_000;

/// The public form of a proposal id (`p_…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProposalPublicId(pub ProposalId);

impl PublicId for ProposalPublicId {
    const PREFIX: &'static str = "p";
    fn raw(self) -> i64 {
        self.0.0
    }
    fn from_raw(raw: i64) -> Self {
        Self(ProposalId(raw))
    }
}

/// The mail commands of the local API. See the [module docs](self).
#[derive(Clone)]
pub struct CoreCommandHandler {
    core: CoreHandle,
    cache: Arc<TokenCache>,
}

impl fmt::Debug for CoreCommandHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreCommandHandler").finish_non_exhaustive()
    }
}

/// The error for commands this build can't run yet.
pub fn not_available(command: &str) -> CommandError {
    CommandError::new(
        ErrorCode::Internal,
        format!(
            "`mazemail {}` is not available in this version of {}",
            command.replace('.', " "),
            mazemail_types::PRODUCT_NAME
        ),
    )
}

/// Maps a core error to what the client sees. Internal details are logged, not sent.
pub(crate) fn core_error(error: Error) -> CommandError {
    match error {
        Error::NotFound { entity, id } => {
            CommandError::not_found(format!("{entity} {id} not found"))
        }
        Error::InvalidCommand(reason) => CommandError::invalid_args(reason),
        Error::Query(error) => CommandError::invalid_args(format!("invalid search query: {error}")),
        Error::PermissionDenied { reason } => {
            CommandError::new(ErrorCode::PermissionDenied, reason)
        }
        Error::Closed => CommandError::new(
            ErrorCode::Unavailable,
            format!("{} is shutting down", mazemail_types::PRODUCT_NAME),
        ),
        other => {
            tracing::error!(error = %other, "a local API command failed");
            CommandError::internal(format!(
                "the command failed inside {}",
                mazemail_types::PRODUCT_NAME
            ))
        }
    }
}

fn store_error(error: store::Error) -> CommandError {
    core_error(error.into())
}

fn rfc3339_time(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Whether a message is visible to the request: its account is in scope and, for a
/// folder-scoped token, one of its copies is in an allowed folder. Detached messages
/// (no copy anywhere) are never visible.
fn message_in_scope(scope: &EffectiveScope, detail: &MessageDetail) -> bool {
    if !scope.allows_account(detail.account) || detail.locations.is_empty() {
        return false;
    }
    match scope.folders() {
        None => true,
        Some(folders) => detail.locations.iter().any(|l| folders.contains(&l.folder)),
    }
}

/// The first copy of a message in an allowed folder.
fn visible_folder(scope: &EffectiveScope, detail: &MessageDetail) -> Option<FolderId> {
    detail
        .locations
        .iter()
        .map(|l| l.folder)
        .find(|folder| scope.allows_folder(detail.account, *folder))
}

/// A visible message, or `not_found`.
fn visible_message(
    reader: &Reader,
    scope: &EffectiveScope,
    id: MessageId,
) -> Result<MessageDetail, CommandError> {
    reader
        .message(id)
        .map_err(store_error)?
        .filter(|detail| message_in_scope(scope, detail))
        .ok_or_else(|| CommandError::not_found(format!("no message {id}")))
}

/// The visible messages of a thread, oldest first, or `not_found` if there are none.
fn visible_thread(
    reader: &Reader,
    scope: &EffectiveScope,
    thread: ThreadId,
) -> Result<Vec<MessageDetail>, CommandError> {
    let mut visible = Vec::new();
    for id in reader.thread_message_ids(thread).map_err(store_error)? {
        if let Some(detail) = reader.message(id).map_err(store_error)?
            && message_in_scope(scope, &detail)
        {
            visible.push(detail);
        }
    }
    if visible.is_empty() {
        return Err(CommandError::not_found(format!("no thread {thread}")));
    }
    Ok(visible)
}

/// A message (`m_…`) or thread (`t_…`) id as typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Message(MessageId),
    Thread(ThreadId),
}

fn parse_target(text: &str) -> Result<Target, CommandError> {
    let text = text.trim();
    if let Ok(id) = MessageId::parse_public(text) {
        return Ok(Target::Message(id));
    }
    if let Ok(id) = ThreadId::parse_public(text) {
        return Ok(Target::Thread(id));
    }
    Err(CommandError::invalid_args(format!(
        "{text:?} is not a message (m_…) or thread (t_…) id"
    )))
}

/// The visible message ids named by `ids` (threads expand to their visible
/// messages), deduplicated in order.
fn resolve_messages(
    reader: &Reader,
    scope: &EffectiveScope,
    ids: &[String],
) -> Result<Vec<MessageId>, CommandError> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for text in ids {
        let messages = match parse_target(text)? {
            Target::Message(id) => vec![visible_message(reader, scope, id)?.id],
            Target::Thread(id) => visible_thread(reader, scope, id)?
                .into_iter()
                .map(|detail| detail.id)
                .collect(),
        };
        for id in messages {
            if seen.insert(id) {
                out.push(id);
            }
        }
    }
    Ok(out)
}

/// A visible folder, or `not_found`.
fn visible_folder_row(
    reader: &Reader,
    scope: &EffectiveScope,
    id: FolderId,
) -> Result<Folder, CommandError> {
    reader
        .folder(id)
        .map_err(store_error)?
        .filter(|folder| scope.allows_folder(folder.account, folder.id))
        .ok_or_else(|| CommandError::not_found(format!("no folder {id}")))
}

/// Tag names (or keywords) to tag ids, case-insensitively.
fn resolve_tags(reader: &Reader, names: &[String]) -> Result<Vec<TagId>, CommandError> {
    let tags = reader.tags().map_err(store_error)?;
    names
        .iter()
        .map(|name| {
            tags.iter()
                .find(|tag| {
                    tag.name.eq_ignore_ascii_case(name) || tag.keyword.eq_ignore_ascii_case(name)
                })
                .map(|tag| tag.id)
                .ok_or_else(|| {
                    CommandError::invalid_args(format!(
                        "unknown tag {name:?} (see `mazemail tags`)"
                    ))
                })
        })
        .collect()
}

/// The command ids of a [`MailCommand`] for the affected list.
fn command_ids(command: &MailCommand) -> Vec<MessageId> {
    match command {
        MailCommand::SetFlags { ids, .. }
        | MailCommand::SetTags { ids, .. }
        | MailCommand::Move { ids, .. }
        | MailCommand::Copy { ids, .. }
        | MailCommand::Archive { ids }
        | MailCommand::Delete { ids, .. } => ids.clone(),
        _ => Vec::new(),
    }
}

fn public_ids(ids: &[MessageId]) -> Vec<String> {
    ids.iter().map(ToString::to_string).collect()
}

fn output(
    data: &impl serde::Serialize,
    affected: Vec<String>,
) -> Result<CommandOutput, CommandError> {
    Ok(CommandOutput::new(data)?.with_affected(affected))
}

impl CoreCommandHandler {
    /// A handler over `core`, resolving tokens through `cache`.
    pub fn new(core: CoreHandle, cache: Arc<TokenCache>) -> Self {
        Self { core, cache }
    }

    fn reader(&self) -> Result<Reader, CommandError> {
        self.core.store().reader().map_err(store_error)
    }

    /// The store id of the request's token.
    fn store_token(&self, ctx: &RequestContext) -> Result<TokenId, CommandError> {
        let record = ctx
            .token
            .as_ref()
            .ok_or_else(|| CommandError::new(ErrorCode::NotSignedIn, "not signed in"))?;
        if let Some(id) = self.cache.store_id(&record.id) {
            return Ok(id);
        }
        self.reader()?
            .api_token_by_hash(record.hash.as_bytes())
            .map_err(store_error)?
            .map(|row| row.id)
            .ok_or_else(|| CommandError::new(ErrorCode::NotSignedIn, "the token is unknown"))
    }

    /// Stores a proposal with its items, audited like core's own proposals.
    async fn propose(
        &self,
        token: TokenId,
        title: String,
        items: Vec<(MailCommand, Option<String>)>,
    ) -> Result<ProposalId, CommandError> {
        let mut rows = Vec::with_capacity(items.len());
        for (command, reason) in &items {
            let command_json = serde_json::to_string(command).map_err(|error| {
                CommandError::internal(format!("cannot encode a proposal: {error}"))
            })?;
            rows.push(NewProposalItem {
                command_json,
                reason: reason.clone(),
            });
        }
        let names: Vec<&'static str> = items.iter().map(|(c, _)| c.name()).collect();
        let origin = Origin::Agent { token }.as_text();
        self.core
            .store()
            .write(move |w| {
                let id = w.create_proposal(Some(token), &title, &rows)?;
                let detail = json!({ "commands": names, "proposal": id.0 }).to_string();
                w.audit(&origin, "proposed", Some(&detail))?;
                Ok(id)
            })
            .await
            .map_err(store_error)
    }

    /// Runs or proposes a state change, as the dispatcher decided.
    async fn mutate(
        &self,
        ctx: &RequestContext,
        command: MailCommand,
    ) -> Result<CommandOutput, CommandError> {
        let token = self.store_token(ctx)?;
        let ids = public_ids(&command_ids(&command));
        let result = match ctx.mode {
            ExecutionMode::Propose => {
                let title = proposal_title(&command);
                let proposal = self.propose(token, title, vec![(command, None)]).await?;
                MutationOutput {
                    outcome: MutationOutcome::Proposed,
                    proposal: Some(ProposalPublicId(proposal).to_public()),
                    affected: ids,
                }
            }
            ExecutionMode::Execute => {
                let receipt = self
                    .core
                    .apply(command, Origin::Agent { token })
                    .await
                    .map_err(core_error)?;
                match receipt.outcome {
                    CommandOutcome::Proposed(proposal) => MutationOutput {
                        outcome: MutationOutcome::Proposed,
                        proposal: Some(ProposalPublicId(proposal).to_public()),
                        affected: ids,
                    },
                    CommandOutcome::Applied | CommandOutcome::Requested => MutationOutput {
                        outcome: MutationOutcome::Applied,
                        proposal: None,
                        affected: public_ids(&receipt.messages),
                    },
                }
            }
        };
        let affected = result.affected.clone();
        output(&result, affected)
    }

    /// Every visible message matching `query`, newest first, at most `limit`, with
    /// the body snippet for messages that were the best match of their thread.
    async fn search_messages(
        &self,
        scope: &EffectiveScope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(MessageDetail, Option<Snippet>)>, CommandError> {
        let single_account = (scope.accounts().len() == 1)
            .then(|| scope.accounts().iter().next().copied())
            .flatten();
        let page = limit.clamp(1, 500);
        let mut opts = SearchOpts {
            limit: page,
            offset: 0,
            sort: SearchSort::NewestFirst,
            scope: single_account.map(ListScope::Account),
            facets: false,
            snippets: true,
            utc_offset: None,
        };
        let mut found: Vec<(MessageDetail, Option<Snippet>)> = Vec::new();
        for _ in 0..MAX_SEARCH_PAGES {
            let results = self
                .core
                .search(query, opts.clone())
                .await
                .map_err(core_error)?;
            let reader = self.reader()?;
            for thread in &results.threads {
                for id in &thread.matched {
                    let Some(detail) = reader.message(*id).map_err(store_error)? else {
                        continue;
                    };
                    if !message_in_scope(scope, &detail) {
                        continue;
                    }
                    let snippet = (*id == thread.best_message)
                        .then(|| thread.body.clone())
                        .flatten();
                    found.push((detail, snippet));
                }
            }
            let exhausted = results.threads.len() < opts.limit;
            if exhausted || found.len() >= limit {
                break;
            }
            opts.offset += opts.limit;
        }
        found.sort_by(|(a, _), (b, _)| b.date.cmp(&a.date).then(b.id.cmp(&a.id)));
        found.truncate(limit);
        Ok(found)
    }

    async fn search(
        &self,
        ctx: &RequestContext,
        args: SearchArgs,
    ) -> Result<CommandOutput, CommandError> {
        let limit = usize::try_from(args.limit)
            .unwrap_or(usize::MAX)
            .clamp(1, 10_000);
        let found = self
            .search_messages(&ctx.scope, args.query.expose(), limit)
            .await?;
        let reader = self.reader()?;
        let tags: HashMap<TagId, String> = reader
            .tags()
            .map_err(store_error)?
            .into_iter()
            .map(|tag| (tag.id, tag.name))
            .collect();
        let hits: Vec<SearchHit> = found
            .into_iter()
            .map(|(detail, snippet)| {
                let flags = detail.effective_flags();
                SearchHit {
                    id: detail.id.to_string(),
                    thread: detail.thread.to_string(),
                    account: detail.account.to_string(),
                    folder: visible_folder(&ctx.scope, &detail).map(|f| f.to_string()),
                    date: rfc3339_time(detail.date),
                    from: Sensitive::new(
                        detail
                            .from
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_default(),
                    ),
                    subject: Sensitive::new(detail.subject.clone().unwrap_or_default()),
                    snippet: snippet
                        .map(|s| s.fragment)
                        .or_else(|| detail.snippet.clone())
                        .filter(|s| !s.is_empty())
                        .map(Sensitive::new),
                    unread: !flags.contains(Flags::SEEN),
                    flagged: flags.contains(Flags::FLAGGED),
                    tags: detail
                        .tags
                        .iter()
                        .filter_map(|id| tags.get(id).cloned())
                        .collect(),
                    has_attachments: detail.has_attachments,
                }
            })
            .collect();
        let affected = hits.iter().map(|hit| hit.id.clone()).collect();
        output(&hits, affected)
    }

    async fn show(
        &self,
        ctx: &RequestContext,
        args: ShowArgs,
    ) -> Result<CommandOutput, CommandError> {
        let target = parse_target(&args.id)?;
        let details = {
            let reader = self.reader()?;
            match target {
                Target::Message(id) => vec![visible_message(&reader, &ctx.scope, id)?],
                Target::Thread(id) => visible_thread(&reader, &ctx.scope, id)?,
            }
        };
        let thread = details.first().map(|d| d.thread).unwrap_or(ThreadId(0));
        let affected: Vec<String> = details.iter().map(|d| d.id.to_string()).collect();

        if args.format == ShowFormat::Raw {
            let Target::Message(_) = target else {
                return Err(CommandError::invalid_args(
                    "--format raw needs a message id (m_…), not a thread",
                ));
            };
            let detail = &details[0];
            let blob = detail.blob.ok_or_else(|| {
                CommandError::new(
                    ErrorCode::Unavailable,
                    "the message body hasn't been downloaded yet; try again shortly",
                )
            })?;
            let store = self.core.store().clone();
            let raw = tokio::task::spawn_blocking(move || store.blobs().read(&blob))
                .await
                .map_err(|error| core_error(error.into()))?
                .map_err(store_error)?;
            let meta = MessageMeta {
                id: detail.id.to_string(),
                from: Sensitive::new(
                    detail
                        .from
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                ),
                to: detail
                    .to
                    .iter()
                    .map(|a| Sensitive::new(a.to_string()))
                    .collect(),
                date: rfc3339_time(detail.date),
                subject: Sensitive::new(detail.subject.clone().unwrap_or_default()),
                attachments: Vec::new(),
            };
            let data = ShowOutput {
                id: args.id,
                thread: thread.to_string(),
                format: ShowFormat::Raw,
                content: Sensitive::new(String::from_utf8_lossy(&raw).into_owned()),
                messages: vec![meta],
            };
            return output(&data, affected);
        }

        let kind = match args.format {
            ShowFormat::Text => TextKind::Plain,
            ShowFormat::Md | ShowFormat::Raw => TextKind::Markdown,
        };
        let mut rendered = Vec::with_capacity(details.len());
        let mut metas = Vec::with_capacity(details.len());
        for detail in &details {
            let view = self.core.message(detail.id).await.map_err(core_error)?;
            let body = view.rendered();
            let note = match &view.body {
                Body::Ready(_) => "",
                Body::Pending => "(The body hasn't been downloaded yet.)",
                Body::Unreadable => "(The body can't be read.)",
            };
            let attachments: &[mazemail_mime::AttachmentMeta] =
                body.map_or(&[], |b| b.attachments.as_slice());
            rendered.push(render::message(
                kind,
                &RenderedMessage {
                    subject: view.subject.as_deref(),
                    from: view.from.as_ref(),
                    to: &view.to,
                    cc: &view.cc,
                    date: rfc3339_time(view.date),
                    attachments,
                    body: body.map(|b| &b.reader),
                    note,
                },
            ));
            metas.push(MessageMeta {
                id: view.id.to_string(),
                from: Sensitive::new(
                    view.from
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                ),
                to: view
                    .to
                    .iter()
                    .map(|a| Sensitive::new(a.to_string()))
                    .collect(),
                date: rfc3339_time(view.date),
                subject: Sensitive::new(view.subject.clone().unwrap_or_default()),
                attachments: attachments
                    .iter()
                    .filter(|a| a.counts_as_attachment())
                    .map(|a| ApiAttachment {
                        id: format!("{}.{}", view.id, a.part_path),
                        filename: Sensitive::new(a.filename.clone().unwrap_or_default()),
                        content_type: a.content_type.clone(),
                        size: u64::try_from(a.size).unwrap_or(u64::MAX),
                    })
                    .collect(),
            });
        }
        let content = match target {
            Target::Message(_) => rendered.pop().unwrap_or_default(),
            Target::Thread(_) => render::thread(
                kind,
                details.first().and_then(|d| d.subject.as_deref()),
                &rendered,
            ),
        };
        let data = ShowOutput {
            id: args.id,
            thread: thread.to_string(),
            format: args.format,
            content: Sensitive::new(content),
            messages: metas,
        };
        output(&data, affected)
    }

    fn folders(
        &self,
        ctx: &RequestContext,
        args: &FoldersArgs,
    ) -> Result<CommandOutput, CommandError> {
        if let Some(account) = args.account
            && !ctx.scope.allows_account(account)
        {
            return Err(CommandError::not_found(format!("no account {account}")));
        }
        let reader = self.reader()?;
        let all = reader.folders(args.account).map_err(store_error)?;
        let by_id: HashMap<FolderId, &Folder> = all.iter().map(|f| (f.id, f)).collect();
        let path = |folder: &Folder| {
            let mut parts = vec![folder.display_name.clone()];
            let mut parent = folder.parent;
            // Bounded: a malformed parent cycle can't loop forever.
            for _ in 0..64 {
                let Some(id) = parent else { break };
                let Some(row) = by_id.get(&id) else { break };
                parts.push(row.display_name.clone());
                parent = row.parent;
            }
            parts.reverse();
            parts.join("/")
        };
        let infos: Vec<FolderInfo> = all
            .iter()
            .filter(|f| ctx.scope.allows_folder(f.account, f.id))
            .map(|f| FolderInfo {
                id: f.id.to_string(),
                account: f.account.to_string(),
                name: f.display_name.clone(),
                path: path(f),
                role: f.role.map(|r| r.as_str().to_owned()),
                unread: f.unread,
                total: f.total,
            })
            .collect();
        output(&infos, Vec::new())
    }

    fn tags(&self) -> Result<CommandOutput, CommandError> {
        let tags: Vec<TagInfo> = self
            .reader()?
            .tags()
            .map_err(store_error)?
            .into_iter()
            .map(|tag| TagInfo {
                id: tag.id.to_string(),
                name: tag.name,
                color: tag.color,
            })
            .collect();
        output(&tags, Vec::new())
    }

    /// Contacts. Collected addresses come from the mail of every account, so a token
    /// limited to some accounts or folders only gets address book entries.
    fn contacts(
        &self,
        ctx: &RequestContext,
        args: &ContactsArgs,
    ) -> Result<CommandOutput, CommandError> {
        let reader = self.reader()?;
        let all_accounts: BTreeSet<AccountId> = reader
            .accounts()
            .map_err(store_error)?
            .iter()
            .map(|a| a.id)
            .collect();
        let restricted = ctx.scope.folders().is_some() || *ctx.scope.accounts() != all_accounts;
        let limit = usize::try_from(args.limit)
            .unwrap_or(usize::MAX)
            .clamp(1, 500);
        let contacts: Vec<ContactInfo> = reader
            .search_contacts(args.query.expose().trim(), limit.saturating_mul(4))
            .map_err(store_error)?
            .into_iter()
            .filter(|c| !restricted || c.book != "collected")
            .take(limit)
            .map(|c| ContactInfo {
                name: c.display_name.map(Sensitive::new),
                email: Sensitive::new(c.email),
            })
            .collect();
        output(&contacts, Vec::new())
    }

    fn identities(
        &self,
        ctx: &RequestContext,
        args: &IdentitiesArgs,
    ) -> Result<CommandOutput, CommandError> {
        if let Some(account) = args.account
            && !ctx.scope.allows_account(account)
        {
            return Err(CommandError::not_found(format!("no account {account}")));
        }
        let identities: Vec<IdentityInfo> = self
            .reader()?
            .identities(args.account)
            .map_err(store_error)?
            .into_iter()
            .filter(|i| ctx.scope.allows_account(i.account))
            .map(|i| IdentityInfo {
                id: i.id.to_string(),
                account: i.account.to_string(),
                name: Sensitive::new(i.input.display_name.clone().unwrap_or_default()),
                email: Sensitive::new(i.input.email.clone()),
                default: i.input.is_default,
            })
            .collect();
        output(&identities, Vec::new())
    }

    fn archive_command(
        &self,
        ctx: &RequestContext,
        args: &ArchiveArgs,
    ) -> Result<MailCommand, CommandError> {
        let ids = resolve_messages(&self.reader()?, &ctx.scope, &args.ids)?;
        Ok(MailCommand::Archive { ids })
    }

    fn move_command(
        &self,
        ctx: &RequestContext,
        ids: &[String],
        to: FolderId,
    ) -> Result<MailCommand, CommandError> {
        let reader = self.reader()?;
        visible_folder_row(&reader, &ctx.scope, to)?;
        let ids = resolve_messages(&reader, &ctx.scope, ids)?;
        Ok(MailCommand::Move {
            ids,
            to,
            from: None,
        })
    }

    fn tag_command(
        &self,
        ctx: &RequestContext,
        ids: &[String],
        add: &[String],
        remove: &[String],
    ) -> Result<MailCommand, CommandError> {
        let reader = self.reader()?;
        let ids = resolve_messages(&reader, &ctx.scope, ids)?;
        Ok(MailCommand::SetTags {
            ids,
            add: resolve_tags(&reader, add)?,
            remove: resolve_tags(&reader, remove)?,
        })
    }

    fn flag_command(
        &self,
        ctx: &RequestContext,
        ids: &[String],
        flag: Flags,
        set: bool,
    ) -> Result<MailCommand, CommandError> {
        let ids = resolve_messages(&self.reader()?, &ctx.scope, ids)?;
        let (add, remove) = if set {
            (flag, Flags::empty())
        } else {
            (Flags::empty(), flag)
        };
        Ok(MailCommand::SetFlags { ids, add, remove })
    }

    async fn propose_batch(
        &self,
        ctx: &RequestContext,
        args: ProposeArgs,
    ) -> Result<CommandOutput, CommandError> {
        let token = self.store_token(ctx)?;
        let mut items = Vec::with_capacity(args.actions.len());
        let mut affected = Vec::new();
        for action in &args.actions {
            let (command, reason) = match action {
                ProposedAction::Archive { ids, reason } => (
                    self.archive_command(ctx, &ArchiveArgs { ids: ids.clone() })?,
                    reason,
                ),
                ProposedAction::Move { ids, to, reason } => {
                    (self.move_command(ctx, ids, *to)?, reason)
                }
                ProposedAction::Tag {
                    ids,
                    add,
                    remove,
                    reason,
                } => (self.tag_command(ctx, ids, add, remove)?, reason),
                ProposedAction::Flag {
                    ids,
                    flagged,
                    reason,
                } => (
                    self.flag_command(ctx, ids, Flags::FLAGGED, *flagged)?,
                    reason,
                ),
                ProposedAction::MarkRead { ids, read, reason } => {
                    (self.flag_command(ctx, ids, Flags::SEEN, *read)?, reason)
                }
                ProposedAction::Unsubscribe { .. } => {
                    return Err(CommandError::new(
                        ErrorCode::Internal,
                        format!(
                            "unsubscribe proposals are not available in this version of {}",
                            mazemail_types::PRODUCT_NAME
                        ),
                    ));
                }
            };
            affected.extend(public_ids(&command_ids(&command)));
            items.push((command, reason.as_ref().map(|r| r.expose().clone())));
        }
        let count = u32::try_from(items.len()).unwrap_or(u32::MAX);
        let title = args
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                if count == 1 {
                    "1 proposed action".to_owned()
                } else {
                    format!("{count} proposed actions")
                }
            });
        let proposal = self.propose(token, title, items).await?;
        affected.sort();
        affected.dedup();
        output(
            &ProposalOutput {
                proposal: ProposalPublicId(proposal).to_public(),
                items: count,
            },
            affected,
        )
    }

    async fn export(
        &self,
        ctx: &RequestContext,
        args: ExportArgs,
    ) -> Result<CommandOutput, CommandError> {
        if !args.to.is_absolute() {
            return Err(CommandError::invalid_args(
                "the export path must be absolute (the CLI resolves it)",
            ));
        }
        let mut found: Vec<MessageDetail> = self
            .search_messages(&ctx.scope, args.query.expose(), MAX_EXPORT)
            .await?
            .into_iter()
            .map(|(detail, _)| detail)
            .collect();
        // Oldest first, like a folder in Thunderbird.
        found.sort_by_key(|detail| (detail.date, detail.id));
        let affected: Vec<String> = found.iter().map(|d| d.id.to_string()).collect();
        let store = self.core.store().clone();
        let target = args.to.clone();
        let format = args.format;
        let written = self
            .core
            .runtime()
            .spawn_blocking(move || export::write(&store, &found, format, &target, None))
            .await
            .map_err(|_| CommandError::internal("the export stopped unexpectedly"))?
            .map_err(|error| match error {
                export::ExportError::Exists(_) | export::ExportError::Io { .. } => {
                    CommandError::invalid_args(error.to_string())
                }
                other => {
                    tracing::error!(error = %other, "export failed");
                    CommandError::internal(format!(
                        "the export failed inside {}",
                        mazemail_types::PRODUCT_NAME
                    ))
                }
            })?;
        if written.without_body > 0 {
            tracing::info!(
                skipped = written.without_body,
                "export skipped messages without a stored body"
            );
        }
        output(
            &ExportOutput {
                path: args.to,
                messages: written.messages,
            },
            affected,
        )
    }

    async fn import(
        &self,
        ctx: &RequestContext,
        args: ImportArgs,
    ) -> Result<CommandOutput, CommandError> {
        let imported = import::run(&self.core, ctx, args).await?;
        output(
            &ImportOutput {
                messages: imported.stats.imported,
                folders: imported.stats.folders_created,
            },
            Vec::new(),
        )
    }
}

#[async_trait]
impl CommandHandler for CoreCommandHandler {
    async fn handle(
        &self,
        ctx: &RequestContext,
        command: Command,
    ) -> Result<CommandOutput, CommandError> {
        match command {
            Command::Search(args) => self.search(ctx, args).await,
            Command::Show(args) => self.show(ctx, args).await,
            Command::Folders(args) => self.folders(ctx, &args),
            Command::Tags(_) => self.tags(),
            Command::Contacts(args) => self.contacts(ctx, &args),
            Command::Identities(args) => self.identities(ctx, &args),
            Command::Archive(args) => {
                let command = self.archive_command(ctx, &args)?;
                self.mutate(ctx, command).await
            }
            Command::Move(MoveArgs { to, ids }) => {
                let command = self.move_command(ctx, &ids, to)?;
                self.mutate(ctx, command).await
            }
            Command::Tag(TagArgs { add, remove, ids }) => {
                let command = self.tag_command(ctx, &ids, &add, &remove)?;
                self.mutate(ctx, command).await
            }
            Command::Flag(FlagArgs { clear, ids }) => {
                let command = self.flag_command(ctx, &ids, Flags::FLAGGED, !clear)?;
                self.mutate(ctx, command).await
            }
            Command::Read(ReadArgs { unread, ids }) => {
                let command = self.flag_command(ctx, &ids, Flags::SEEN, !unread)?;
                self.mutate(ctx, command).await
            }
            Command::Propose(args) => self.propose_batch(ctx, args).await,
            Command::Export(args) => self.export(ctx, args).await,
            Command::Import(args) => self.import(ctx, args).await,
            Command::AccountList(_) => output(&super::accounts::list(&self.core, ctx)?, Vec::new()),
            Command::AccountAdd(args) => {
                let added = super::accounts::add(&self.core, args).await?;
                let id = added.id.clone();
                output(&added, vec![id])
            }
            Command::AccountRemove(args) => {
                let removed = super::accounts::remove(&self.core, ctx, &args).await?;
                let id = removed.id.clone();
                output(&removed, vec![id])
            }
            Command::Sync(args) => {
                let synced = super::accounts::sync(&self.core, ctx, &args)?;
                let ids = synced.accounts.clone();
                output(&synced, ids)
            }
            command @ (Command::AttachmentSave(_)
            | Command::DraftNew(_)
            | Command::DraftEdit(_)
            | Command::RuleAdd(_)
            | Command::Send(_)) => Err(not_available(command.name())),
        }
    }

    async fn status(&self, ctx: &RequestContext) -> Value {
        if ctx.token.is_none() {
            return Value::Null;
        }
        let accounts = match self.core.accounts() {
            Ok(accounts) => accounts,
            Err(error) => {
                tracing::warn!(%error, "cannot read accounts for status");
                return Value::Null;
            }
        };
        let visible: Vec<Value> = accounts
            .iter()
            .filter(|a| ctx.scope.allows_account(a.id))
            .map(|a| json!({ "id": a.id.to_string(), "name": a.name, "kind": a.kind.as_str() }))
            .collect();
        json!({ "accounts": visible })
    }
}

//! Message rules (filters, ADR-029): run on new mail, run on a folder now and the
//! rules list.
//!
//! The format and the matching live in the `rules` crate; this module runs the actions
//! as ordinary [`MailCommand`]s with [`Origin::Rule`], inside the same write job that
//! picks the messages, so pending ops, the optimistic change and the audit entry
//! (origin `rule:<id>`) are the same as for a command from the UI.
//!
//! # New mail
//!
//! The sync engine signals every batch of an account's actor; the runner (started by
//! [`CoreHandle::open`] with a sync engine) then calls [`CoreHandle::run_arrival_rules`]
//! for the account. Rules with an arrival trigger, enabled, of the account or of every
//! account, run in their order on the mail that arrived in the account's Inbox:
//!
//! - **Which mail.** Each watched folder (the Inbox; on Gmail, All Mail, for messages
//!   with the Inbox label) has a cursor: the highest UID the rules have seen, per
//!   UIDVALIDITY. Messages above it are new. A folder's first cursor (a new account, a
//!   UIDVALIDITY change) starts at the end of what the first sync found, so old mail is
//!   never filtered on its own; run the rules on a folder for that.
//! - **Once.** Processed messages are recorded (`rule_message`), so a message a rule
//!   moved out and the user moves back isn't processed again, and a crash between the
//!   actions and the cursor update can't run them twice: the actions, the record and
//!   the cursor commit together.
//! - **Bodies.** A rule with a `body` condition needs the message text. A message
//!   without its body yet is held back (the cursor waits) and its body is requested;
//!   after [`BODY_WAIT`] it is evaluated without it.
//! - **Failures.** Each action runs in its own savepoint: one that fails (a folder that
//!   is gone) is skipped and logged in the audit log (`rule_failed`), the others run.
//!
//! # Run now
//!
//! [`CoreHandle::run_rules`] runs the enabled rules of the folder's account, or one
//! rule (enabled or not, to try it), over every message in a folder. Bodies are not
//! waited for: `body` conditions see the text of the messages that have it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use mazemail_types::{AccountId, Flags, FolderId, FolderRole, MessageId, TagId};
use serde_json::Value;
use store::{
    BodyState, Folder, MessageDetail, Queries, RuleCursor, RuleId, RuleInput, RuleTrigger, WriteTx,
};
use tokio::sync::broadcast;

use crate::commands::{self, Applied, ExecContext, MailCommand, Origin};
use crate::folders::folder_path;
use crate::handle::{CoreHandle, map_store};
use crate::sync_bridge::FetchPriority;
use crate::{Error, Result};

/// New messages one write job looks at, at most.
pub const ARRIVAL_BATCH: usize = 200;

/// Messages one write job of [`CoreHandle::run_rules`] looks at, at most.
pub const MANUAL_BATCH: usize = 500;

/// How long new mail waits for its body when a rule has a `body` condition.
pub const BODY_WAIT: Duration = Duration::from_secs(5 * 60);

/// A rule for lists and the CLI.
#[derive(Clone, PartialEq, Eq)]
pub struct RuleSummary {
    /// The rule.
    pub id: RuleId,
    /// Its account; `None`: every account.
    pub account: Option<AccountId>,
    /// Its name.
    pub name: String,
    /// Its place in the order.
    pub ordinal: i64,
    /// Whether it runs.
    pub enabled: bool,
    /// When it runs.
    pub trigger: RuleTrigger,
    /// The conditions, for people (`from contains "anna" and …`). Contains addresses.
    pub conditions: String,
    /// The actions, for people (`move to Lists`).
    pub actions: Vec<String>,
    /// What keeps it from running as intended.
    pub problems: Vec<String>,
    /// Whether it may be enabled.
    pub runnable: bool,
    /// Imported from Thunderbird.
    pub imported: bool,
    /// The import's note, if any.
    pub note: Option<String>,
}

impl fmt::Debug for RuleSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuleSummary")
            .field("id", &self.id)
            .field("account", &self.account)
            .field("enabled", &self.enabled)
            .field("runnable", &self.runnable)
            .finish_non_exhaustive()
    }
}

impl RuleSummary {
    fn new(rule: &store::Rule) -> Self {
        let input = &rule.input;
        let extra: Value = serde_json::from_str(&input.condition_json).unwrap_or(Value::Null);
        let (conditions, actions, problems, runnable) =
            match rules::Rule::parse(&input.condition_json, &input.actions_json) {
                Ok(parsed) => (
                    parsed.condition.to_string(),
                    parsed.actions.iter().map(ToString::to_string).collect(),
                    parsed.problems().iter().map(ToString::to_string).collect(),
                    parsed.is_runnable(),
                ),
                Err(err) => ("?".to_owned(), Vec::new(), vec![err.to_string()], false),
            };
        Self {
            id: rule.id,
            account: input.account,
            name: input.name.clone(),
            ordinal: input.ordinal,
            enabled: input.enabled,
            trigger: input.trigger,
            conditions,
            actions,
            problems,
            runnable,
            imported: extra.get("source").and_then(Value::as_str) == Some("thunderbird"),
            note: extra
                .get("import_note")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }
}

/// A rule to add.
#[derive(Clone, PartialEq, Eq)]
pub struct NewRule {
    /// Its account; `None`: every account.
    pub account: Option<AccountId>,
    /// Its name.
    pub name: String,
    /// Conditions and actions.
    pub rule: rules::Rule,
    /// Whether it runs on new mail right away.
    pub enabled: bool,
}

impl fmt::Debug for NewRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewRule")
            .field("account", &self.account)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// What a run of rules did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleRunReport {
    /// Messages the rules looked at.
    pub examined: usize,
    /// Messages at least one rule acted on.
    pub matched: usize,
    /// Commands run (one per rule and action, covering all its messages).
    pub actions: usize,
    /// Actions skipped or failed (see the audit log).
    pub failed: usize,
    /// Messages changed.
    pub messages: Vec<MessageId>,
}

impl RuleRunReport {
    fn merge(&mut self, other: Self) {
        self.examined += other.examined;
        self.matched += other.matched;
        self.actions += other.actions;
        self.failed += other.failed;
        self.messages.extend(other.messages);
        self.messages.sort();
        self.messages.dedup();
    }
}

/// A sync batch of an account, for the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Arrival {
    /// The account.
    pub(crate) account: AccountId,
    /// Whether messages were added (else only bodies or counts changed).
    pub(crate) added: bool,
}

/// The broadcast side of the arrival signals.
pub(crate) fn arrivals_channel() -> broadcast::Sender<Arrival> {
    broadcast::channel(256).0
}

/// A rule that can run, parsed.
struct Loaded {
    id: RuleId,
    rule: rules::Rule,
}

/// The rules of `account` (and of every account) that `wanted` keeps, in order,
/// parsed; rules that can't run are left out with a warning.
fn load(
    reader: &impl Queries,
    account: AccountId,
    wanted: impl Fn(&store::Rule) -> bool,
) -> store::Result<Vec<Loaded>> {
    let mut loaded = Vec::new();
    for rule in reader.rules(Some(account))? {
        if !wanted(&rule) {
            continue;
        }
        match rules::Rule::parse(&rule.input.condition_json, &rule.input.actions_json) {
            Ok(parsed) if parsed.is_runnable() => loaded.push(Loaded {
                id: rule.id,
                rule: parsed,
            }),
            Ok(_) => tracing::warn!(rule = %rule.id, "an enabled rule can't run; skipped"),
            Err(err) => tracing::warn!(rule = %rule.id, %err, "an enabled rule can't be read"),
        }
    }
    Ok(loaded)
}

fn arrival_trigger(trigger: RuleTrigger) -> bool {
    matches!(
        trigger,
        RuleTrigger::Arrival | RuleTrigger::ArrivalBeforeJunk
    )
}

/// What the rules see of a message.
fn facts(detail: &MessageDetail) -> rules::Message<'_> {
    rules::Message {
        from: detail.from.as_ref(),
        to: &detail.to,
        cc: &detail.cc,
        subject: detail.subject.as_deref(),
        body: detail.snippet.as_deref(),
        list_id: detail.list_id.as_deref(),
        has_attachment: detail.has_attachments,
    }
}

/// The planned actions of a batch, grouped into commands.
#[derive(Default)]
struct Batch {
    /// `(rank, rule, action, messages)` in first-seen order.
    groups: Vec<(u8, RuleId, rules::Action, Vec<MessageId>)>,
    report: RuleRunReport,
}

impl Batch {
    /// Plans `rules` for one message.
    fn add(&mut self, rules: &[Loaded], detail: &MessageDetail) {
        self.report.examined += 1;
        let planned = rules::plan(rules.iter().map(|r| (r.id, &r.rule)), &facts(detail));
        if planned.is_empty() {
            return;
        }
        self.report.matched += 1;
        for step in planned {
            let rank = match step.action.kind {
                rules::ActionKind::CopyToFolder => 1,
                ref kind if kind.moves() => 2,
                _ => 0,
            };
            match self.groups.iter_mut().find(|(r, rule, action, _)| {
                *r == rank && *rule == step.rule && action == step.action
            }) {
                Some((_, _, _, ids)) => ids.push(detail.id),
                None => self
                    .groups
                    .push((rank, step.rule, step.action.clone(), vec![detail.id])),
            }
        }
    }

    /// Runs the commands: flags and tags, then copies, then moves; each in its own
    /// savepoint. `from` is the folder the messages are in.
    fn run(
        mut self,
        w: &mut WriteTx<'_>,
        ctx: ExecContext,
        account: AccountId,
        from: FolderId,
        applied: &mut Applied,
    ) -> store::Result<RuleRunReport> {
        self.groups.sort_by_key(|(rank, ..)| *rank);
        if self.groups.is_empty() {
            return Ok(self.report);
        }
        let folders = w.folders(Some(account))?;
        let mut changed = Vec::new();
        for (_, rule, action, ids) in &self.groups {
            let origin = Origin::Rule { rule: *rule }.as_text();
            let command = match command_for(w, &folders, action, ids, from) {
                Ok(Some(command)) => command,
                Ok(None) => continue,
                Err(reason) => {
                    self.report.failed += 1;
                    let detail = serde_json::json!({
                        "action": action.kind.as_str(),
                        "messages": ids.len(),
                        "reason": reason,
                    })
                    .to_string();
                    w.audit(&origin, "rule_skipped", Some(&detail))?;
                    continue;
                }
            };
            match w.attempt(|w| commands::apply_in_tx(w, &command, &origin, ctx)) {
                Ok(done) => {
                    self.report.actions += 1;
                    changed.extend(done.messages.iter().copied());
                    applied.threads.extend(done.threads);
                    applied.queued.extend(done.queued);
                    applied.ops.extend(done.ops);
                }
                Err(err) => {
                    self.report.failed += 1;
                    tracing::warn!(%rule, %err, action = action.kind.as_str(), "a rule action failed");
                    let detail = serde_json::json!({
                        "action": action.kind.as_str(),
                        "messages": ids.len(),
                        "reason": err.to_string(),
                    })
                    .to_string();
                    w.audit(&origin, "rule_failed", Some(&detail))?;
                }
            }
        }
        changed.sort();
        changed.dedup();
        self.report.messages = changed;
        Ok(self.report)
    }
}

/// The command for one action on `ids` (in `from`), `Ok(None)` if there is nothing to
/// do, or why it can't run.
fn command_for(
    reader: &impl Queries,
    folders: &[Folder],
    action: &rules::Action,
    ids: &[MessageId],
    from: FolderId,
) -> std::result::Result<Option<MailCommand>, String> {
    use rules::ActionKind as K;
    let ids = ids.to_vec();
    let flags = |add: Flags, remove: Flags| {
        Some(MailCommand::SetFlags {
            ids: ids.clone(),
            add,
            remove,
        })
    };
    Ok(match &action.kind {
        K::MarkRead => flags(Flags::SEEN, Flags::empty()),
        K::MarkUnread => flags(Flags::empty(), Flags::SEEN),
        K::MarkFlagged => flags(Flags::FLAGGED, Flags::empty()),
        K::AddTag => {
            let tag = resolve_tag(reader, action.value().unwrap_or_default())
                .map_err(|err| err.to_string())?
                .ok_or("the tag doesn't exist")?;
            Some(MailCommand::SetTags {
                ids,
                add: vec![tag],
                remove: Vec::new(),
            })
        }
        K::MoveToFolder | K::CopyToFolder => {
            let to = resolve_folder(folders, action.value().unwrap_or_default())
                .ok_or("the folder doesn't exist in this account")?;
            if to == from {
                return Ok(None);
            }
            if action.kind == K::CopyToFolder {
                Some(MailCommand::Copy { ids, to })
            } else {
                Some(MailCommand::Move {
                    ids,
                    to,
                    from: Some(from),
                })
            }
        }
        K::Archive => Some(MailCommand::Archive { ids }),
        K::Junk => Some(MailCommand::Junk { ids }),
        K::Trash => {
            // Delete on mail already in the Trash deletes for good: rules never do.
            let trash: HashSet<FolderId> = folders
                .iter()
                .filter(|f| f.role == Some(FolderRole::Trash))
                .map(|f| f.id)
                .collect();
            if trash.is_empty() {
                return Err("the account has no Trash folder".to_owned());
            }
            let mut keep = Vec::new();
            for id in ids {
                let in_trash = reader
                    .message(id)
                    .map_err(|err| err.to_string())?
                    .is_some_and(|d| d.locations.iter().any(|l| trash.contains(&l.folder)));
                if !in_trash {
                    keep.push(id);
                }
            }
            (!keep.is_empty()).then_some(MailCommand::Delete {
                ids: keep,
                permanent: false,
            })
        }
        K::Stop | K::Other(_) => None,
    })
}

/// The folder a rule names: its server name, its path (`Customers/Acme`), or either
/// ignoring case (`INBOX/Lists` for `Inbox/Lists`).
pub fn resolve_folder(folders: &[Folder], path: &str) -> Option<FolderId> {
    let path = path.trim().trim_matches('/');
    if path.is_empty() {
        return None;
    }
    let paths: Vec<(FolderId, Option<&str>, Option<String>)> = folders
        .iter()
        .map(|f| (f.id, f.remote_name.as_deref(), folder_path(folders, f.id)))
        .collect();
    let exact = paths
        .iter()
        .find(|(_, remote, _)| *remote == Some(path))
        .or_else(|| paths.iter().find(|(_, _, p)| p.as_deref() == Some(path)));
    let loose = || {
        paths.iter().find(|(_, remote, p)| {
            remote.is_some_and(|r| r.eq_ignore_ascii_case(path))
                || p.as_deref().is_some_and(|p| p.eq_ignore_ascii_case(path))
        })
    };
    exact.or_else(loose).map(|(id, ..)| *id)
}

/// The tag a rule names: its IMAP keyword or its name (ignoring case).
fn resolve_tag(reader: &impl Queries, name: &str) -> store::Result<Option<TagId>> {
    let name = name.trim();
    if let Some(tag) = reader.tag_by_keyword(name)? {
        return Ok(Some(tag.id));
    }
    Ok(reader
        .tags()?
        .into_iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .map(|t| t.id))
}

/// What one arrival pass did.
#[derive(Default)]
struct ArrivalPass {
    applied: Applied,
    report: RuleRunReport,
    /// Messages held back for their body.
    deferred: Vec<MessageId>,
    /// A batch was full: run again.
    more: bool,
}

/// The arrival rules of `account` on its new mail, one batch per watched folder (see
/// the [module docs](self)). `give_up` are held-back messages that waited long enough.
fn arrival_in_tx(
    w: &mut WriteTx<'_>,
    account: AccountId,
    ctx: ExecContext,
    give_up: &HashSet<MessageId>,
) -> store::Result<ArrivalPass> {
    let rules = load(w, account, |r| {
        r.input.enabled && arrival_trigger(r.input.trigger)
    })?;
    let needs_body = rules.iter().any(|r| r.rule.needs_body());
    let mut pass = ArrivalPass::default();
    for inbox in w.folders_with_role(FolderRole::Inbox, Some(account))? {
        let watched = if inbox.is_label {
            match w
                .folders_with_role(FolderRole::All, Some(account))?
                .into_iter()
                .next()
            {
                Some(all) => all,
                None => continue,
            }
        } else {
            inbox.clone()
        };
        // Not synced completely yet: the first cursor waits for the first sync.
        let (Some(validity), Some(next)) = (watched.sync.uidvalidity, watched.sync.uidnext) else {
            continue;
        };
        let cursor = match w.rule_cursor(watched.id)? {
            Some(cursor) if cursor.uidvalidity == validity => cursor,
            _ => {
                let cursor = RuleCursor {
                    uidvalidity: validity,
                    last_uid: next.saturating_sub(1),
                };
                w.set_rule_cursor(watched.id, cursor)?;
                cursor
            }
        };
        let candidates = w.locations_after_uid(watched.id, cursor.last_uid, ARRIVAL_BATCH)?;
        let full = candidates.len() == ARRIVAL_BATCH;
        let mut last = cursor.last_uid;
        let mut held = false;
        let mut processed = Vec::new();
        let mut batch = Batch::default();
        for (uid, id) in candidates {
            if !rules.is_empty() && !w.rule_processed(id)? {
                let detail = w.message(id)?;
                if let Some(detail) =
                    detail.filter(|d| d.locations.iter().any(|l| l.folder == inbox.id))
                {
                    if needs_body && detail.body_state == BodyState::None && !give_up.contains(&id)
                    {
                        pass.deferred.push(id);
                        held = true;
                        continue;
                    }
                    batch.add(&rules, &detail);
                    processed.push(id);
                }
            }
            if !held {
                last = uid;
            }
        }
        let report = batch.run(w, ctx, account, inbox.id, &mut pass.applied)?;
        pass.report.merge(report);
        w.mark_rule_processed(&processed)?;
        if last != cursor.last_uid {
            w.set_rule_cursor(
                watched.id,
                RuleCursor {
                    uidvalidity: validity,
                    last_uid: last,
                },
            )?;
            pass.more |= full;
        }
    }
    Ok(pass)
}

/// One batch of [`CoreHandle::run_rules`]: the messages of `folder` after `after`.
fn manual_in_tx(
    w: &mut WriteTx<'_>,
    folder: FolderId,
    only: Option<RuleId>,
    after: MessageId,
    ctx: ExecContext,
) -> store::Result<(Applied, RuleRunReport, Option<MessageId>)> {
    let target = w.folder(folder)?.ok_or(store::Error::NotFound {
        entity: "folder",
        id: folder.to_string(),
    })?;
    let rules = load(w, target.account, |r| match only {
        Some(id) => r.id == id,
        None => r.input.enabled,
    })?;
    let ids = w.folder_messages_after(folder, after, MANUAL_BATCH)?;
    let next = (ids.len() == MANUAL_BATCH)
        .then(|| ids.last().copied())
        .flatten();
    let mut batch = Batch::default();
    if !rules.is_empty() {
        for id in ids {
            if let Some(detail) = w.message(id)? {
                batch.add(&rules, &detail);
            }
        }
    }
    let mut applied = Applied::default();
    let report = batch.run(w, ctx, target.account, folder, &mut applied)?;
    Ok((applied, report, next))
}

impl CoreHandle {
    /// The rules of one account (with the rules for every account), or all rules, in
    /// their order.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub fn rules(&self, account: Option<AccountId>) -> Result<Vec<RuleSummary>> {
        self.ensure_open()?;
        let reader = self.store().reader()?;
        Ok(reader
            .rules(account)?
            .iter()
            .map(RuleSummary::new)
            .collect())
    }

    /// One rule.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if there is no such rule.
    pub fn rule(&self, id: RuleId) -> Result<RuleSummary> {
        self.ensure_open()?;
        let rule = self
            .store()
            .reader()?
            .rule(id)?
            .ok_or_else(|| rule_not_found(id))?;
        Ok(RuleSummary::new(&rule))
    }

    /// Enables or disables a rule. Only a rule Maze can run may be enabled (see
    /// [`rules::Rule::problems`]).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`], or [`Error::InvalidRule`] for a rule that can't run.
    pub async fn set_rule_enabled(&self, id: RuleId, enabled: bool) -> Result<RuleSummary> {
        self.ensure_open()?;
        let summary = self.rule(id)?;
        if enabled && !summary.runnable {
            let problems: Vec<&str> = summary.problems.iter().map(String::as_str).collect();
            return Err(Error::InvalidRule(format!(
                "the rule \"{}\" can't be enabled: {}",
                summary.name,
                problems.join("; ")
            )));
        }
        self.store()
            .write(move |w| w.set_rule_enabled(id, enabled))
            .await
            .map_err(map_store)?;
        self.rule(id)
    }

    /// Deletes a rule.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if there is no such rule.
    pub async fn delete_rule(&self, id: RuleId) -> Result<()> {
        self.ensure_open()?;
        self.store()
            .write(move |w| w.delete_rule(id))
            .await
            .map_err(map_store)
    }

    /// Adds a rule for new mail at the end of the order.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRule`] for a rule that can't run.
    pub async fn add_rule(&self, new: NewRule) -> Result<RuleId> {
        self.ensure_open()?;
        if !new.rule.is_runnable() {
            let problems: Vec<String> = new
                .rule
                .problems()
                .iter()
                .map(ToString::to_string)
                .collect();
            return Err(Error::InvalidRule(format!(
                "the rule can't run: {}",
                problems.join("; ")
            )));
        }
        let id = self
            .store()
            .write(move |w| {
                let ordinal = w
                    .rules(None)?
                    .iter()
                    .map(|r| r.input.ordinal)
                    .max()
                    .map_or(0, |max| max + 1);
                w.create_rule(&RuleInput {
                    account: new.account,
                    name: new.name,
                    ordinal,
                    enabled: new.enabled,
                    trigger: RuleTrigger::Arrival,
                    condition_json: new.rule.condition_json(),
                    actions_json: new.rule.actions_json(),
                })
            })
            .await
            .map_err(map_store)?;
        Ok(id)
    }

    /// Runs rules over every message in `folder` now: the enabled rules of its account,
    /// or only `rule` (enabled or not, so it can be tried before enabling it).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for an unknown folder or rule, [`Error::InvalidRule`] for a
    /// rule that can't run or belongs to another account.
    pub async fn run_rules(&self, folder: FolderId, rule: Option<RuleId>) -> Result<RuleRunReport> {
        self.ensure_open()?;
        let target = self
            .store()
            .reader()?
            .folder(folder)?
            .ok_or(Error::NotFound {
                entity: "folder",
                id: folder.to_string(),
            })?;
        if let Some(id) = rule {
            let summary = self.rule(id)?;
            if summary.account.is_some_and(|a| a != target.account) {
                return Err(Error::InvalidRule(format!(
                    "the rule \"{}\" belongs to another account",
                    summary.name
                )));
            }
            if !summary.runnable {
                return Err(Error::InvalidRule(format!(
                    "the rule \"{}\" can't run: {}",
                    summary.name,
                    summary.problems.join("; ")
                )));
            }
        }
        let ctx = self.exec_context();
        let mut report = RuleRunReport::default();
        let mut after = MessageId(0);
        loop {
            let (applied, done, next) = self
                .store()
                .write_weighted(MANUAL_BATCH, move |w| {
                    manual_in_tx(w, folder, rule, after, ctx)
                })
                .await
                .map_err(map_store)?;
            self.after_write(&applied.threads, &applied.queued);
            report.merge(done);
            match next {
                Some(next) => after = next,
                None => break,
            }
        }
        Ok(report)
    }

    /// Runs the arrival rules of `account` on its new mail (see the [module
    /// docs](self)). The core does this by itself as mail arrives; it is public for
    /// headless use and tests.
    ///
    /// # Errors
    ///
    /// The store failed.
    pub async fn run_arrival_rules(&self, account: AccountId) -> Result<RuleRunReport> {
        Ok(self.arrival_pass(account, HashSet::new()).await?.0)
    }

    /// [`CoreHandle::run_arrival_rules`], returning the messages held back for their
    /// body; `give_up` are held-back messages that waited long enough.
    async fn arrival_pass(
        &self,
        account: AccountId,
        give_up: HashSet<MessageId>,
    ) -> Result<(RuleRunReport, Vec<MessageId>)> {
        self.ensure_open()?;
        let ctx = self.exec_context();
        let give_up = Arc::new(give_up);
        let mut report = RuleRunReport::default();
        loop {
            let give_up = Arc::clone(&give_up);
            let pass = self
                .store()
                .write_weighted(ARRIVAL_BATCH, move |w| {
                    arrival_in_tx(w, account, ctx, &give_up)
                })
                .await
                .map_err(map_store)?;
            self.after_write(&pass.applied.threads, &pass.applied.queued);
            report.merge(pass.report);
            if !pass.more {
                if report.actions > 0 {
                    tracing::debug!(%account, messages = report.messages.len(), "rules acted on new mail");
                }
                return Ok((report, pass.deferred));
            }
        }
    }
}

/// A rule condition from a search query (`mazemail rule add --when`): `from:`, `to:`,
/// `cc:`, `subject:`, `list:`, `has:attachment` and free text (the message text), each
/// optionally negated with `-`; all of them (`a b`) or any (`a OR b`), not both.
///
/// # Errors
///
/// [`Error::Query`] for a query that doesn't parse, [`Error::InvalidRule`] for one a
/// rule can't express.
pub fn condition_from_query(query: &str) -> Result<rules::Condition> {
    use search::query::{Query, Term};

    fn term(query: &Query) -> std::result::Result<rules::Term, String> {
        let (negated, term) = match query {
            Query::Term(term) => (false, term),
            Query::Not(inner) => match inner.as_ref() {
                Query::Term(term) => (true, term),
                _ => return Err("only single terms can be negated".to_owned()),
            },
            _ => return Err("use either all terms or OR between them, not both".to_owned()),
        };
        let (field, op, value) = match term {
            Term::From(text) => (rules::Field::From, rules::Op::Contains, text.as_str()),
            Term::To(text) => (rules::Field::To, rules::Op::Contains, text.as_str()),
            Term::Cc(text) => (rules::Field::Cc, rules::Op::Contains, text.as_str()),
            Term::Subject(text) => (rules::Field::Subject, rules::Op::Contains, text.as_str()),
            Term::Text(text) => (rules::Field::Body, rules::Op::Contains, text.as_str()),
            Term::List(list) => (rules::Field::ListId, rules::Op::Contains, list.as_str()),
            Term::HasAttachment => (rules::Field::HasAttachment, rules::Op::Is, "true"),
            _ => {
                return Err(
                    "rules understand from:, to:, cc:, subject:, list:, has:attachment and free \
                     text"
                        .to_owned(),
                );
            }
        };
        let op = if negated {
            op.negated().unwrap_or(rules::Op::IsNot)
        } else {
            op
        };
        Ok(rules::Term::new(field, op, value))
    }

    let parsed = search::query::parse(query)?.query;
    let condition = match &parsed {
        Query::And(items) if items.is_empty() => {
            return Err(Error::InvalidRule("the rule needs a condition".to_owned()));
        }
        Query::And(items) => rules::Condition::all(
            items
                .iter()
                .map(term)
                .collect::<std::result::Result<_, _>>()
                .map_err(Error::InvalidRule)?,
        ),
        Query::Or(items) => rules::Condition::any(
            items
                .iter()
                .map(term)
                .collect::<std::result::Result<_, _>>()
                .map_err(Error::InvalidRule)?,
        ),
        single => rules::Condition::all(vec![term(single).map_err(Error::InvalidRule)?]),
    };
    Ok(condition)
}

fn rule_not_found(id: RuleId) -> Error {
    Error::NotFound {
        entity: "rule",
        id: id.to_string(),
    }
}

/// Runs the arrival rules as the sync engine reports batches (see the [module
/// docs](self)): every account once at the start, then each account with a batch.
pub(crate) fn spawn_runner(core: &CoreHandle, mut arrivals: broadcast::Receiver<Arrival>) {
    let weak = core.downgrade();
    let task = core.runtime().spawn(async move {
        // Held back for their body: message → (account, since).
        let mut held: HashMap<MessageId, (AccountId, tokio::time::Instant)> = HashMap::new();
        let mut due: Option<BTreeSet<AccountId>> = None;
        loop {
            let Some(core) = CoreHandle::upgrade(&weak) else {
                return;
            };
            let accounts: Vec<AccountId> = match due.take() {
                Some(accounts) => accounts.into_iter().collect(),
                None => match core.accounts() {
                    Ok(accounts) => accounts.into_iter().map(|a| a.id).collect(),
                    Err(_) => return,
                },
            };
            for account in accounts {
                let now = tokio::time::Instant::now();
                let give_up: HashSet<MessageId> = held
                    .iter()
                    .filter(|(_, (a, since))| {
                        *a == account && now.duration_since(*since) >= BODY_WAIT
                    })
                    .map(|(id, _)| *id)
                    .collect();
                match core.arrival_pass(account, give_up).await {
                    Ok((_, deferred)) => {
                        held.retain(|id, (a, _)| *a != account || deferred.contains(id));
                        for id in deferred {
                            if let std::collections::hash_map::Entry::Vacant(entry) = held.entry(id)
                            {
                                entry.insert((account, now));
                                core.sync_bridge().fetch_body(id, FetchPriority::Background);
                            }
                        }
                    }
                    Err(Error::Closed) => return,
                    Err(err) => tracing::warn!(%err, %account, "running the rules on new mail"),
                }
            }
            drop(core);
            // Wait for a batch that matters, or until a held-back message waited long
            // enough. Signals that come together are handled together.
            let mut next = BTreeSet::new();
            let mut all = false;
            while !all && next.is_empty() {
                let wake = held.values().map(|(_, since)| *since + BODY_WAIT).min();
                let first = tokio::select! {
                    arrival = arrivals.recv() => Some(arrival),
                    () = sleep_until(wake) => None,
                };
                let Some(first) = first else {
                    next.extend(held.values().map(|(a, _)| *a));
                    continue;
                };
                let mut arrival = first;
                loop {
                    match arrival {
                        Ok(a) => {
                            if a.added || held.values().any(|(h, _)| *h == a.account) {
                                next.insert(a.account);
                            }
                        }
                        // Missed signals: look at every account.
                        Err(broadcast::error::RecvError::Lagged(_)) => all = true,
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                    arrival = match arrivals.try_recv() {
                        Ok(a) => Ok(a),
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            Err(broadcast::error::RecvError::Lagged(n))
                        }
                        Err(_) => break,
                    };
                }
            }
            due = (!all).then_some(next);
        }
    });
    core.push_task(task);
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: i64, parent: Option<i64>, remote: &str, display: &str) -> Folder {
        Folder {
            id: FolderId(id),
            account: AccountId(1),
            parent: parent.map(FolderId),
            remote_name: Some(remote.to_owned()),
            display_name: display.to_owned(),
            role: None,
            is_label: false,
            sync: store::FolderSyncState::default(),
            unread: 0,
            total: 0,
            ordinal: 0,
            favorite: false,
        }
    }

    #[test]
    fn queries_become_conditions() {
        let all = condition_from_query("from:anna -subject:spam has:attachment").unwrap();
        assert_eq!(
            all.to_string(),
            "from contains \"anna\" and subject doesn't contain \"spam\" and has attachments"
        );
        let any = condition_from_query("list:dev.example.org OR invoice").unwrap();
        assert_eq!(any.matching, rules::Match::Any);
        assert_eq!(any.terms[1].field, rules::Field::Body);
        assert!(condition_from_query("is:unread").is_err());
        assert!(condition_from_query("a (b OR c)").is_err());
        assert!(condition_from_query("").is_err());
    }

    #[test]
    fn folders_resolve_by_server_name_path_or_ignoring_case() {
        let folders = vec![
            folder(1, None, "INBOX", "Inbox"),
            folder(2, Some(1), "INBOX.Lists", "Lists"),
            folder(3, None, "Customers", "Customers"),
            folder(4, Some(3), "Customers/Acme", "Acme"),
        ];
        assert_eq!(resolve_folder(&folders, "INBOX.Lists"), Some(FolderId(2)));
        assert_eq!(resolve_folder(&folders, "Inbox/Lists"), Some(FolderId(2)));
        assert_eq!(resolve_folder(&folders, "INBOX/Lists"), Some(FolderId(2)));
        assert_eq!(
            resolve_folder(&folders, "Customers/Acme"),
            Some(FolderId(4))
        );
        assert_eq!(
            resolve_folder(&folders, "/customers/acme/"),
            Some(FolderId(4))
        );
        assert_eq!(resolve_folder(&folders, "Nope"), None);
        assert_eq!(resolve_folder(&folders, ""), None);
    }
}

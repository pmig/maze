//! The `rule` commands (ADR-029) over [`CoreHandle`]'s rules.
//!
//! A rule of an account outside the request's scope is `not_found`, like a rule that
//! doesn't exist. Rules for every account are visible to every token; adding one
//! needs a token whose scope covers every account (otherwise `--account` is required).

use local_api::registry::args::{RuleAction, RuleAddArgs, RuleIdArgs, RuleListArgs, RuleRunArgs};
use local_api::registry::output::{RuleDeleted, RuleInfo, RuleOutput, RuleRunOutput};
use local_api::{CommandError, CommandOutput, RequestContext};
use mazemail_types::{AccountId, FolderRole, PublicId};
use store::{Queries as _, RuleId};

use super::handler::{core_error, output, store_error, visible_folder_row};
use crate::folders::folder_path;
use crate::handle::CoreHandle;
use crate::rules::{NewRule, RuleSummary, condition_from_query};

/// The public form of a rule id (`r_…`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RulePublicId(pub RuleId);

impl PublicId for RulePublicId {
    const PREFIX: &'static str = "r";
    fn raw(self) -> i64 {
        self.0.0
    }
    fn from_raw(raw: i64) -> Self {
        Self(RuleId(raw))
    }
}

fn info(rule: RuleSummary) -> RuleInfo {
    RuleInfo {
        id: RulePublicId(rule.id).to_public(),
        name: rule.name,
        account: rule.account.map(|a| a.to_string()),
        enabled: rule.enabled,
        trigger: rule.trigger.as_str().to_owned(),
        conditions: rule.conditions,
        actions: rule.actions.join(", "),
        problems: rule.problems,
        runnable: rule.runnable,
        imported: rule.imported,
        note: rule.note,
    }
}

fn in_scope(ctx: &RequestContext, rule: &RuleSummary) -> bool {
    rule.account.is_none_or(|a| ctx.scope.allows_account(a))
}

/// The rule `id` names, if visible.
fn visible_rule(
    core: &CoreHandle,
    ctx: &RequestContext,
    id: &str,
) -> Result<RuleSummary, CommandError> {
    let not_found = || CommandError::not_found(format!("no rule {id}"));
    let parsed = RulePublicId::parse_public(id).map_err(|_| not_found())?;
    match core.rule(parsed.0) {
        Ok(rule) if in_scope(ctx, &rule) => Ok(rule),
        Ok(_) | Err(crate::Error::NotFound { .. }) => Err(not_found()),
        Err(err) => Err(core_error(err)),
    }
}

pub(super) fn list(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: &RuleListArgs,
) -> Result<CommandOutput, CommandError> {
    if args.account.is_some_and(|a| !ctx.scope.allows_account(a)) {
        return Err(CommandError::not_found("no such account"));
    }
    let rules: Vec<RuleInfo> = core
        .rules(args.account)
        .map_err(core_error)?
        .into_iter()
        .filter(|r| in_scope(ctx, r))
        .map(info)
        .collect();
    let ids = rules.iter().map(|r| r.id.clone()).collect();
    output(&rules, ids)
}

pub(super) fn show(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: &RuleIdArgs,
) -> Result<CommandOutput, CommandError> {
    let rule = info(visible_rule(core, ctx, &args.id)?);
    let id = rule.id.clone();
    output(&rule, vec![id])
}

pub(super) async fn set_enabled(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: &RuleIdArgs,
    enabled: bool,
) -> Result<CommandOutput, CommandError> {
    let rule = visible_rule(core, ctx, &args.id)?;
    let rule = info(
        core.set_rule_enabled(rule.id, enabled)
            .await
            .map_err(core_error)?,
    );
    let id = rule.id.clone();
    output(&rule, vec![id])
}

pub(super) async fn delete(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: &RuleIdArgs,
) -> Result<CommandOutput, CommandError> {
    let rule = visible_rule(core, ctx, &args.id)?;
    core.delete_rule(rule.id).await.map_err(core_error)?;
    let deleted = RuleDeleted {
        id: RulePublicId(rule.id).to_public(),
        name: rule.name,
    };
    let id = deleted.id.clone();
    output(&deleted, vec![id])
}

fn run_output(report: &crate::RuleRunReport) -> RuleRunOutput {
    RuleRunOutput {
        examined: report.examined as u64,
        matched: report.matched as u64,
        actions: report.actions as u64,
        failed: report.failed as u64,
        affected: report.messages.iter().map(ToString::to_string).collect(),
    }
}

pub(super) async fn run(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: &RuleRunArgs,
) -> Result<CommandOutput, CommandError> {
    let reader = core.store().reader().map_err(store_error)?;
    let folder = visible_folder_row(&reader, &ctx.scope, args.folder)?;
    drop(reader);
    let rule = match &args.id {
        Some(id) => Some(visible_rule(core, ctx, id)?.id),
        None => None,
    };
    let report = core.run_rules(folder.id, rule).await.map_err(core_error)?;
    let out = run_output(&report);
    let affected = out.affected.clone();
    output(&out, affected)
}

pub(super) async fn add(
    core: &CoreHandle,
    ctx: &RequestContext,
    args: RuleAddArgs,
) -> Result<CommandOutput, CommandError> {
    let reader = core.store().reader().map_err(store_error)?;
    let mut account: Option<AccountId> = args.account;
    if account.is_some_and(|a| !ctx.scope.allows_account(a)) {
        return Err(CommandError::not_found("no such account"));
    }
    // Folders name the account: a move or copy makes the rule that account's.
    let mut actions = Vec::with_capacity(args.actions.len());
    for action in &args.actions {
        use rules::{Action, ActionKind};
        actions.push(match action {
            RuleAction::Archive => Action::new(ActionKind::Archive),
            RuleAction::MarkRead => Action::new(ActionKind::MarkRead),
            RuleAction::Flag => Action::new(ActionKind::MarkFlagged),
            RuleAction::Trash => Action::new(ActionKind::Trash),
            RuleAction::Stop => Action::new(ActionKind::Stop),
            RuleAction::Move { to } | RuleAction::Copy { to } => {
                let folder = visible_folder_row(&reader, &ctx.scope, *to)?;
                match account {
                    Some(a) if a != folder.account => {
                        return Err(CommandError::invalid_args(format!(
                            "folder {to} is in another account than the rule"
                        )));
                    }
                    _ => account = Some(folder.account),
                }
                let folders = reader.folders(Some(folder.account)).map_err(store_error)?;
                let path = folder_path(&folders, folder.id)
                    .ok_or_else(|| CommandError::not_found(format!("no folder {to}")))?;
                let kind = if matches!(action, RuleAction::Move { .. }) {
                    ActionKind::MoveToFolder
                } else {
                    ActionKind::CopyToFolder
                };
                Action::with_value(kind, path)
            }
            RuleAction::Tag { tag } => {
                let found = reader
                    .tags()
                    .map_err(store_error)?
                    .into_iter()
                    .find(|t| {
                        t.name.eq_ignore_ascii_case(tag) || t.keyword.eq_ignore_ascii_case(tag)
                    })
                    .ok_or_else(|| {
                        CommandError::invalid_args(format!(
                            "unknown tag {tag:?} (see `mazemail tags`)"
                        ))
                    })?;
                Action::with_value(ActionKind::AddTag, found.keyword)
            }
        });
    }
    if account.is_none() {
        let all = core
            .accounts()
            .map_err(core_error)?
            .iter()
            .all(|a| ctx.scope.allows_account(a.id));
        if !all {
            return Err(CommandError::invalid_args(
                "a rule for every account needs a token for every account; pass --account",
            ));
        }
    }
    let condition = condition_from_query(args.when.expose()).map_err(core_error)?;
    let inboxes = reader
        .folders_with_role(FolderRole::Inbox, account)
        .map_err(store_error)?;
    drop(reader);
    let name = args.name.trim().to_owned();
    if name.is_empty() {
        return Err(CommandError::invalid_args("name: empty"));
    }
    let id = core
        .add_rule(NewRule {
            account,
            name: name.clone(),
            rule: rules::Rule { condition, actions },
            enabled: true,
        })
        .await
        .map_err(core_error)?;
    if args.apply_now {
        for inbox in inboxes
            .iter()
            .filter(|f| ctx.scope.allows_folder(f.account, f.id))
        {
            core.run_rules(inbox.id, Some(id))
                .await
                .map_err(core_error)?;
        }
    }
    let out = RuleOutput {
        id: RulePublicId(id).to_public(),
        name,
    };
    let affected = vec![out.id.clone()];
    output(&out, affected)
}

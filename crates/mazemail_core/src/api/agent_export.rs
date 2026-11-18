//! Exporting a mail thread into an agent session's scratch dir (PLAN §4.12).
//!
//! The export goes through the same `show --format md` handler agents use, with a
//! scope of every account.

use std::sync::Arc;

use local_api::registry::args::{ShowArgs, ShowFormat};
use local_api::{
    AccountList as _, Command, CommandError, CommandHandler as _, EffectiveScope, ErrorCode,
    ExecutionMode, PeerInfo, ProcessInfo, RequestContext, TokenScope,
};
use mazemail_types::ThreadId;

use super::{CoreAccountList, CoreCommandHandler, TokenCache};
use crate::handle::CoreHandle;

/// The thread as Markdown, or `None` when it doesn't exist.
pub async fn thread_markdown_for_agents(
    core: &CoreHandle,
    thread: ThreadId,
) -> Result<Option<String>, CommandError> {
    let accounts = CoreAccountList::new(core.clone()).all_accounts();
    let scope = EffectiveScope::resolve(&TokenScope::all(), &accounts);
    if scope.is_empty() {
        return Ok(None);
    }
    let handler = CoreCommandHandler::new(core.clone(), Arc::new(TokenCache::default()));
    let peer = PeerInfo {
        uid: local_api::current_uid(),
        gid: 0,
        process: ProcessInfo {
            pid: i32::try_from(std::process::id()).ok(),
            exe: std::env::current_exe().ok(),
            parent: None,
        },
    };
    let ctx = RequestContext::new(0, Arc::new(peer), None, scope, ExecutionMode::Execute);
    let command = Command::Show(ShowArgs {
        id: thread.to_string(),
        format: ShowFormat::Md,
    });
    match handler.handle(&ctx, command).await {
        Ok(output) => Ok(output.data.into_inner()["content"]
            .as_str()
            .map(str::to_owned)),
        Err(error) if error.code == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

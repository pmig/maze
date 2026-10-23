//! Approvers that need no window.
//!
//! `mazemail --headless` asks on its controlling terminal
//! ([`TerminalApprover`](local_api::TerminalApprover)); without one it denies
//! everything. For scripted end-to-end tests of debug builds,
//! [`TEST_AUTO_APPROVE_ENV`] makes it approve every request as asked. The switch is
//! compiled only with `debug_assertions`: release builds don't contain it at all, so no
//! environment variable can turn approvals off for real users.

use std::sync::Arc;

use local_api::{Approver, DenyAllApprover, TerminalApprover};

/// `MAZEMAIL_TEST_AUTO_APPROVE=1`: approve every login and `ask` command (debug builds
/// only; ignored, and not even read, in release builds).
pub const TEST_AUTO_APPROVE_ENV: &str = "MAZEMAIL_TEST_AUTO_APPROVE";

/// Approves every request exactly as asked. Debug builds only.
#[cfg(debug_assertions)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AutoApprover;

#[cfg(debug_assertions)]
#[async_trait::async_trait]
impl Approver for AutoApprover {
    async fn approve(&self, request: local_api::ApprovalRequest) -> local_api::Decision {
        tracing::warn!(
            command = %request.command,
            profile = %request.profile,
            "auto-approved ({TEST_AUTO_APPROVE_ENV}=1, debug build)"
        );
        local_api::Decision::Approve(request.requested_grant())
    }
}

/// The debug-only auto-approver if [`TEST_AUTO_APPROVE_ENV`] is `1`.
pub fn test_auto_approver() -> Option<Arc<dyn Approver>> {
    #[cfg(debug_assertions)]
    {
        if std::env::var(TEST_AUTO_APPROVE_ENV).is_ok_and(|value| value.trim() == "1") {
            tracing::warn!(
                "INSECURE: every local API approval is granted automatically \
                 ({TEST_AUTO_APPROVE_ENV}=1); this exists only in debug builds, for tests"
            );
            return Some(Arc::new(AutoApprover));
        }
    }
    None
}

/// The approver for `--headless`: the test auto-approver if enabled, else the
/// controlling terminal, else deny everything (with a warning).
pub fn headless_approver() -> Arc<dyn Approver> {
    if let Some(approver) = test_auto_approver() {
        return approver;
    }
    match TerminalApprover::controlling_tty() {
        Ok(approver) => Arc::new(approver),
        Err(error) => {
            tracing::warn!(
                %error,
                "no terminal to ask on: logins and commands that need approval are denied"
            );
            Arc::new(DenyAllApprover)
        }
    }
}

//! The GPUI [`Approver`]: `mazemail login` and commands that need approval ask in a
//! modal dialog of the main window (PLAN §4.11, CONCEPT §10.5).
//!
//! The dialog names the requesting program and its parent (from the socket's peer
//! credentials), the profile, the scope and the lifetime, and offers "Allow once" and
//! "Deny". Closing it any other way denies. Requests are shown one at a time, oldest
//! first; Maze comes to the front for each. A request that the server stops waiting
//! for (its approval timeout) is answered "deny" by the server, whatever the dialog
//! says later.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use async_trait::async_trait;
use gpui::{App, AsyncApp, IntoElement, ParentElement as _, Styled as _, div};
use local_api::{ApprovalKind, ApprovalRequest, Approver, Decision, Ttl};
use mazemail_types::PRODUCT_NAME;
use tokio::sync::oneshot;
use ui::kit::component::{ActiveTheme as _, WindowExt as _, v_flex};

/// One request waiting for the user.
struct Pending {
    request: ApprovalRequest,
    reply: oneshot::Sender<Decision>,
}

/// Hands approval requests from the server (any thread) to the dialog on GPUI's
/// thread.
struct GpuiApprover {
    tx: async_channel::Sender<Pending>,
}

#[async_trait]
impl Approver for GpuiApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let (reply, answer) = oneshot::channel();
        if self.tx.send(Pending { request, reply }).await.is_err() {
            return Decision::Deny;
        }
        answer.await.unwrap_or(Decision::Deny)
    }
}

/// Starts the dialog loop and returns the approver for the local API.
pub(crate) fn start(cx: &mut App) -> Arc<dyn Approver> {
    let (tx, rx) = async_channel::unbounded::<Pending>();
    cx.spawn(async move |cx| {
        while let Ok(pending) = rx.recv().await {
            // The server gave up waiting (timeout, client gone): don't ask any more.
            if pending.reply.is_closed() {
                continue;
            }
            let decision = ask(pending.request, cx).await;
            let _ = pending.reply.send(decision);
        }
    })
    .detach();
    Arc::new(GpuiApprover { tx })
}

/// Shows the dialog and waits for the answer.
async fn ask(request: ApprovalRequest, cx: &mut AsyncApp) -> Decision {
    let (tx, rx) = oneshot::channel();
    let shown = cx.update(|cx| show(&request, tx, cx));
    if !shown {
        tracing::warn!("an approval was denied: no {PRODUCT_NAME} window is open");
        return Decision::Deny;
    }
    rx.await.unwrap_or(Decision::Deny)
}

/// The facts the dialog shows, as `(label, value)` rows. No mail content: command
/// arguments aren't shown.
pub(crate) fn rows(request: &ApprovalRequest) -> Vec<(&'static str, String)> {
    let mut rows = vec![("Program", request.process.describe())];
    if let Some(parent) = &request.process.parent {
        let exe = parent.exe.as_ref().map_or_else(
            || "unknown program".to_owned(),
            |exe| exe.display().to_string(),
        );
        rows.push(("Started by", format!("{exe} (pid {})", parent.pid)));
    }
    match &request.kind {
        ApprovalKind::Login { label, allow_send } => {
            rows.push(("Asks for", "a session token".to_owned()));
            if let Some(label) = label {
                rows.push(("Label", label.clone()));
            }
            rows.push(("Profile", request.profile.to_string()));
            rows.push(("Scope", request.scope.describe()));
            rows.push((
                "Lifetime",
                request
                    .lifetime
                    .map_or_else(|| "no expiry".to_owned(), |d| Ttl::new(d).to_string()),
            ));
            if *allow_send {
                rows.push(("Sending", "allowed (every send still asks)".to_owned()));
            }
        }
        ApprovalKind::Command { .. } => {
            rows.push((
                "Asks to run",
                format!("mazemail {}", request.command.replace('.', " ")),
            ));
            rows.push(("Profile", request.profile.to_string()));
            rows.push(("Scope", request.scope.describe()));
        }
    }
    if let Some(token) = &request.requester_token {
        rows.push(("Holds token", token.to_string()));
    }
    rows
}

fn title(request: &ApprovalRequest) -> String {
    match request.kind {
        ApprovalKind::Login { .. } => format!("Allow a program to use {PRODUCT_NAME}?"),
        ApprovalKind::Command { .. } => format!("Allow this {PRODUCT_NAME} command?"),
    }
}

/// Opens the dialog in the main window; `false` if there is none.
fn show(request: &ApprovalRequest, reply: oneshot::Sender<Decision>, cx: &mut App) -> bool {
    let Some(window) = super::main_window(cx) else {
        return false;
    };
    cx.activate(true);
    let grant = request.requested_grant();
    let title = title(request);
    let rows = rows(request);
    let reply = Rc::new(RefCell::new(Some(reply)));
    let answer = move |reply: &Rc<RefCell<Option<oneshot::Sender<Decision>>>>, decision| {
        if let Some(reply) = reply.borrow_mut().take() {
            let _ = reply.send(decision);
        }
    };
    window
        .update(cx, |_, window, cx| {
            window.open_alert_dialog(cx, move |alert, _, cx| {
                let (allow, deny, close) = (reply.clone(), reply.clone(), reply.clone());
                let grant = grant.clone();
                let muted = cx.theme().muted_foreground;
                alert
                    .title(title.clone())
                    .description(v_flex().gap_1().children(rows.iter().map(|(label, value)| {
                        div()
                            .flex()
                            .gap_2()
                            .child(div().w_24().flex_none().text_color(muted).child(*label))
                            .child(div().flex_1().min_w_0().child(value.clone()))
                            .into_any_element()
                    })))
                    .ok_text("Allow once")
                    .cancel_text("Deny")
                    .show_cancel(true)
                    .on_ok(move |_, _, _| {
                        answer(&allow, Decision::Approve(grant.clone()));
                        true
                    })
                    .on_cancel(move |_, _, _| {
                        answer(&deny, Decision::Deny);
                        true
                    })
                    // Escape, the close button: deny.
                    .on_close(move |_, _, _| answer(&close, Decision::Deny))
            });
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use local_api::{ParentProcess, ProcessInfo, Profile, TokenScope};

    use super::*;

    #[test]
    fn the_dialog_shows_process_profile_scope_and_lifetime() {
        let request = ApprovalRequest {
            process: ProcessInfo {
                pid: Some(42),
                exe: Some("/usr/local/bin/mazemail".into()),
                parent: Some(ParentProcess {
                    pid: 7,
                    exe: Some("/bin/zsh".into()),
                }),
            },
            requester_token: None,
            profile: Profile::ReadOnly,
            scope: TokenScope::all(),
            lifetime: Some(Duration::from_secs(3600)),
            command: "login".into(),
            kind: ApprovalKind::Login {
                label: Some("claude".into()),
                allow_send: false,
            },
        };
        let rows = rows(&request);
        let get = |label: &str| {
            rows.iter()
                .find(|(l, _)| *l == label)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("Program"), Some("/usr/local/bin/mazemail (pid 42)"));
        assert_eq!(get("Started by"), Some("/bin/zsh (pid 7)"));
        assert_eq!(get("Profile"), Some("read-only"));
        assert_eq!(get("Scope"), Some("all accounts"));
        assert_eq!(get("Lifetime"), Some("1h"));
        assert_eq!(get("Label"), Some("claude"));
        assert_eq!(title(&request), "Allow a program to use Maze?");
    }
}

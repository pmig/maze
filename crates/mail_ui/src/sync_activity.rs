//! "Sync activity": the status bar's sync item opens a popover that says exactly what
//! is syncing, per account: the state (with the reason of a problem and "Retry now"),
//! the folders being synced with their progress, the last finished sync, and the work
//! that waits (changes for the server, the outbox, offline downloads).
//!
//! The popover reads [`MailSource::sync_activity`] each time it renders; while it is
//! open the window re-renders twice a second (see `MailWindow`), so it follows the sync
//! live without any per-message events.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    Anchor, App, InteractiveElement as _, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled, Window, div, prelude::FluentBuilder as _,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    popover::Popover,
    v_flex,
};
use time::OffsetDateTime;
use ui::assets::IconName;

use crate::format::format_thousands;
use crate::source::{
    AccountActivity, AccountKind, ActivityState, FolderProgress, FolderStep, MailSource,
};

/// How often the open popover re-reads the activity.
pub(crate) const ACTIVITY_REFRESH: std::time::Duration = std::time::Duration::from_millis(500);

/// `Fn(open)` when the popover opens or closes.
pub(crate) type OpenChange = Rc<dyn Fn(&bool, &mut Window, &mut App)>;

/// The status bar's sync item with its "Sync activity" popover.
#[derive(IntoElement)]
pub(crate) struct SyncActivityItem {
    source: Arc<dyn MailSource>,
    label: SharedString,
    open: bool,
    on_open_change: OpenChange,
}

impl SyncActivityItem {
    /// The item showing `label` ("Up to date", "Work: syncing 3/10"); `open` says
    /// whether the popover is open, `on_open_change` is told when that changes.
    pub(crate) fn new(
        source: Arc<dyn MailSource>,
        label: SharedString,
        open: bool,
        on_open_change: OpenChange,
    ) -> Self {
        Self {
            source,
            label,
            open,
            on_open_change,
        }
    }
}

impl RenderOnce for SyncActivityItem {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let source = self.source;
        let on_open_change = self.on_open_change;
        div().debug_selector(|| "status-sync".to_owned()).child(
            Popover::new("sync-activity")
                .anchor(Anchor::BottomLeft)
                .open(self.open)
                .on_open_change(move |open, window, cx| on_open_change(open, window, cx))
                .trigger(
                    Button::new("status-sync")
                        .ghost()
                        .xsmall()
                        .icon(IconName::RefreshCw)
                        .label(self.label)
                        .tooltip("Sync activity"),
                )
                .content(move |_, _, cx| {
                    let now = OffsetDateTime::now_utc();
                    activity_panel(&source, &source.sync_activity(), now, cx)
                }),
        )
    }
}

/// The popover's content.
fn activity_panel(
    source: &Arc<dyn MailSource>,
    accounts: &[AccountActivity],
    now: OffsetDateTime,
    cx: &App,
) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    v_flex()
        .debug_selector(|| "sync-activity".to_owned())
        .w(gpui::px(380.))
        .gap_3()
        .text_sm()
        .child(div().font_semibold().child("Sync activity"))
        .when(accounts.is_empty(), |this| {
            this.child(
                div()
                    .text_color(muted)
                    .child("No accounts sync yet. Add one with \u{201c}Add Account\u{201d}."),
            )
        })
        .children(
            accounts
                .iter()
                .map(|account| account_section(source, account, now, cx)),
        )
}

fn account_section(
    source: &Arc<dyn MailSource>,
    account: &AccountActivity,
    now: OffsetDateTime,
    cx: &App,
) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let state_color = match &account.state {
        ActivityState::Offline { .. }
        | ActivityState::SignInRequired { .. }
        | ActivityState::Error { .. } => theme.danger,
        _ => muted,
    };
    let id = account.account;
    let retry = account.state.can_retry().then(|| {
        let source = Arc::clone(source);
        div()
            .debug_selector(move || format!("sync-activity-retry-{}", id.0))
            .child(
                Button::new(SharedString::from(format!("sync-activity-retry-{}", id.0)))
                    .xsmall()
                    .label("Retry now")
                    .on_click(move |_, window, cx| {
                        if let Err(error) = source.sync_account_now(id) {
                            tracing::warn!(%error, "retrying an account's sync failed");
                        }
                        window.refresh();
                        cx.stop_propagation();
                    }),
            )
    });
    let icon = match account.kind {
        AccountKind::Gmail | AccountKind::Microsoft => IconName::Mail,
        AccountKind::Imap => IconName::Server,
    };
    v_flex()
        .debug_selector(move || format!("sync-activity-account-{}", id.0))
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Icon::new(icon).size_3p5())
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .font_medium()
                        .child(account.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(state_color)
                        .child(state_label(&account.state)),
                ),
        )
        .when_some(account.state.problem().cloned(), |this, problem| {
            this.child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(problem),
                    )
                    .children(retry),
            )
        })
        .children(
            account
                .folders
                .iter()
                .map(|folder| div().pl_5().text_xs().truncate().child(folder_line(folder))),
        )
        .children(
            detail_lines(account, now)
                .into_iter()
                .map(|line| div().pl_5().text_xs().text_color(muted).child(line)),
        )
}

/// "Syncing", "Up to date", "Offline", ….
pub(crate) fn state_label(state: &ActivityState) -> &'static str {
    match state {
        ActivityState::NotSynced => "Not synced",
        ActivityState::Connecting => "Connecting…",
        ActivityState::Syncing => "Syncing",
        ActivityState::Idle => "Up to date",
        ActivityState::Offline { .. } => "Offline",
        ActivityState::SignInRequired { .. } => "Sign-in required",
        ActivityState::Error { .. } => "Error",
    }
}

/// "INBOX — fetching headers 1,200 / 5,400", "All Mail — re-reading labels".
pub(crate) fn folder_line(folder: &FolderProgress) -> String {
    let step = match (folder.step, folder.total) {
        (FolderStep::Waiting, _) => "waiting".to_owned(),
        (FolderStep::Checking, _) => "checking for changes".to_owned(),
        (FolderStep::ReadingFlags, _) => "reading flags".to_owned(),
        (FolderStep::ReadingLabels, _) => "re-reading labels".to_owned(),
        (FolderStep::FetchingHeaders, Some(total)) => format!(
            "fetching headers {} / {}",
            format_thousands(folder.done),
            format_thousands(total)
        ),
        (FolderStep::FetchingHeaders, None) => format!(
            "fetching headers ({} so far)",
            format_thousands(folder.done)
        ),
    };
    format!("{} \u{2014} {step}", folder.name)
}

/// The last sync and the work that waits, one sentence each.
pub(crate) fn detail_lines(account: &AccountActivity, now: OffsetDateTime) -> Vec<String> {
    let mut lines = Vec::new();
    if account.replaying_ops {
        lines.push("Sending changes to the server…".to_owned());
    }
    if account.pending_ops > 0 {
        let changes = plural(account.pending_ops, "change", "changes");
        let oldest = account
            .oldest_op
            .map(|at| format!(", the oldest from {}", ago(at, now)))
            .unwrap_or_default();
        lines.push(format!("{changes} waiting for the server{oldest}"));
        if account.stuck_ops > 0 {
            lines.push(format!(
                "{} failed too often and {} for you",
                plural(account.stuck_ops, "change", "changes"),
                if account.stuck_ops == 1 {
                    "waits"
                } else {
                    "wait"
                }
            ));
        }
    }
    if account.outbox > 0 {
        lines.push(format!(
            "{} in the outbox",
            plural(account.outbox, "message", "messages")
        ));
    }
    if account.bodies_queued > 0 {
        lines.push(format!(
            "Downloading {} for offline reading",
            plural(account.bodies_queued, "message", "messages")
        ));
    }
    lines.push(match account.last_synced {
        Some(at) => format!("Last synced {}", ago(at, now)),
        None if account.state == ActivityState::NotSynced => "This account isn't synced".to_owned(),
        None => "Not synced since Maze started".to_owned(),
    });
    lines
}

/// "1 change", "1,200 changes".
fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {many}", format_thousands(n))
    }
}

/// "just now", "2 min ago", "3 h ago", "2 days ago".
pub(crate) fn ago(then: OffsetDateTime, now: OffsetDateTime) -> String {
    let secs = (now - then).whole_seconds().max(0);
    match secs {
        0..60 => "just now".to_owned(),
        60..3_600 => format!("{} min ago", secs / 60),
        3_600..86_400 => format!("{} h ago", secs / 3_600),
        _ => match secs / 86_400 {
            1 => "1 day ago".to_owned(),
            days => format!("{days} days ago"),
        },
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn account() -> AccountActivity {
        AccountActivity {
            account: mazemail_types::AccountId(1),
            name: "Work".into(),
            kind: AccountKind::Imap,
            state: ActivityState::Idle,
            folders: Vec::new(),
            replaying_ops: false,
            bodies_queued: 0,
            last_synced: None,
            pending_ops: 0,
            stuck_ops: 0,
            oldest_op: None,
            outbox: 0,
        }
    }

    #[test]
    fn folder_lines_say_what_each_folder_does() {
        let folder = |step, done, total| FolderProgress {
            name: "INBOX".into(),
            step,
            done,
            total,
        };
        assert_eq!(
            folder_line(&folder(FolderStep::FetchingHeaders, 1_200, Some(5_400))),
            "INBOX \u{2014} fetching headers 1,200 / 5,400"
        );
        assert_eq!(
            folder_line(&FolderProgress {
                name: "All Mail".into(),
                ..folder(FolderStep::ReadingLabels, 0, None)
            }),
            "All Mail \u{2014} re-reading labels"
        );
        assert_eq!(
            folder_line(&folder(FolderStep::Waiting, 0, None)),
            "INBOX \u{2014} waiting"
        );
        assert_eq!(
            folder_line(&folder(FolderStep::Checking, 0, None)),
            "INBOX \u{2014} checking for changes"
        );
    }

    #[test]
    fn details_cover_the_last_sync_and_waiting_work() {
        let now = datetime!(2026-10-22 10:12 UTC);
        assert_eq!(
            detail_lines(&account(), now),
            vec!["Not synced since Maze started"]
        );
        let busy = AccountActivity {
            last_synced: Some(datetime!(2026-10-22 10:10 UTC)),
            pending_ops: 3,
            stuck_ops: 1,
            oldest_op: Some(datetime!(2026-10-22 09:00 UTC)),
            outbox: 2,
            bodies_queued: 120,
            replaying_ops: true,
            ..account()
        };
        assert_eq!(
            detail_lines(&busy, now),
            vec![
                "Sending changes to the server…",
                "3 changes waiting for the server, the oldest from 1 h ago",
                "1 change failed too often and waits for you",
                "2 messages in the outbox",
                "Downloading 120 messages for offline reading",
                "Last synced 2 min ago",
            ]
        );
        let unsynced = AccountActivity {
            state: ActivityState::NotSynced,
            ..account()
        };
        assert_eq!(
            detail_lines(&unsynced, now),
            vec!["This account isn't synced"]
        );
    }

    #[test]
    fn relative_times() {
        let now = datetime!(2026-10-22 10:12 UTC);
        assert_eq!(ago(now, now), "just now");
        assert_eq!(ago(datetime!(2026-10-22 10:10 UTC), now), "2 min ago");
        assert_eq!(ago(datetime!(2026-10-22 07:00 UTC), now), "3 h ago");
        assert_eq!(ago(datetime!(2026-10-21 10:00 UTC), now), "1 day ago");
        assert_eq!(ago(datetime!(2026-10-19 10:00 UTC), now), "3 days ago");
        assert_eq!(ago(datetime!(2026-10-23 10:00 UTC), now), "just now");
    }

    #[test]
    fn retry_is_offered_where_it_helps() {
        assert!(ActivityState::Offline { reason: "x".into() }.can_retry());
        assert!(
            ActivityState::Error {
                message: "x".into()
            }
            .can_retry()
        );
        assert!(!ActivityState::SignInRequired { reason: "x".into() }.can_retry());
        assert!(!ActivityState::Idle.can_retry());
        assert_eq!(state_label(&ActivityState::Syncing), "Syncing");
    }
}

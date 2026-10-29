//! The window chrome around the mail columns: spaces bar, agent column, status bar and
//! the title bar's sync button.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, App, InteractiveElement as _, IntoElement, ParentElement,
    RenderOnce, SharedString, StatefulInteractiveElement as _, Styled, Transformation, Window, div,
    percentage, prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    status_bar::StatusBar,
    tooltip::Tooltip,
    v_flex,
};
use mazemail_types::PRODUCT_NAME;
use ui::assets::IconName;
use ui::components::StatusItem;
use ui::theme::scaled;

use crate::actions::{OpenSettings, SyncNow, Toggle, ToggleAgentColumn};
use crate::format::format_disk;
use crate::source::{MailSource, SourceStatus, StorageUsage, SyncState};
use crate::sync_activity::{OpenChange, SyncActivityItem};

/// The slim spaces bar on the far left (CONCEPT §8.2): Mail, Address Book, and at
/// the bottom Extensions and Settings. Mail and Settings (which opens `settings.json`
/// in a tab) work so far.
#[derive(IntoElement)]
pub(crate) struct SpacesBar {
    width: f32,
}

impl SpacesBar {
    /// A spaces bar of the given width.
    pub(crate) fn new(width: f32) -> Self {
        Self { width }
    }
}

impl RenderOnce for SpacesBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let space = |id: &'static str, icon: IconName, tooltip: &'static str| {
            Button::new(id)
                .ghost()
                .icon(Icon::new(icon))
                .tooltip(tooltip)
        };
        v_flex()
            .w(px(self.width))
            .h_full()
            .flex_none()
            .items_center()
            .justify_between()
            .py_2()
            .bg(theme.title_bar)
            .border_r_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .gap_1()
                    .child(space("space-mail", IconName::Mail, "Mail").selected(true))
                    .child(
                        space(
                            "space-contacts",
                            IconName::BookUser,
                            "Address Book (coming later)",
                        )
                        .disabled(true),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        space(
                            "space-extensions",
                            IconName::Puzzle,
                            "Extensions (coming later)",
                        )
                        .disabled(true),
                    )
                    .child(
                        Button::new("space-settings")
                            .ghost()
                            .icon(Icon::new(IconName::Settings))
                            .tooltip_with_action("Settings", &OpenSettings, None)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenSettings), cx);
                            }),
                    ),
            )
    }
}

/// The agent column (CONCEPT §8.2, column 4). AI is opt-in, so until an agent is set
/// up it shows the setup card; collapsed it is a thin strip.
#[derive(IntoElement)]
pub(crate) struct AgentColumn {
    expanded: bool,
    width: f32,
    context: Option<SharedString>,
}

impl AgentColumn {
    /// The expanded column.
    pub(crate) fn expanded(width: f32, context: Option<SharedString>) -> Self {
        Self {
            expanded: true,
            width,
            context,
        }
    }

    /// The collapsed strip.
    pub(crate) fn strip(width: f32) -> Self {
        Self {
            expanded: false,
            width,
            context: None,
        }
    }
}

impl RenderOnce for AgentColumn {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let toggle = Button::new("toggle-agent-column")
            .ghost()
            .xsmall()
            .icon(if self.expanded {
                IconName::PanelRightClose
            } else {
                IconName::PanelRightOpen
            })
            .tooltip_with_action(
                if self.expanded {
                    "Collapse agent column"
                } else {
                    "Expand agent column"
                },
                &ToggleAgentColumn,
                Some("Workspace"),
            )
            .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleAgentColumn), cx));
        let column = v_flex()
            .w(px(self.width))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border);
        if !self.expanded {
            return column.items_center().py_2().gap_2().child(toggle).child(
                Icon::new(IconName::Bot)
                    .small()
                    .text_color(theme.muted_foreground),
            );
        }
        let setup = |id: &'static str, label: &'static str| {
            Button::new(id)
                .outline()
                .small()
                .w_full()
                .label(label)
                .disabled(true)
                .tooltip("Agents need your mail data; they aren't available here")
        };
        column
            .child(
                h_flex()
                    .flex_none()
                    .h(scaled(36., cx))
                    .px_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child("AGENT"),
                    )
                    .child(toggle),
            )
            .child(
                v_flex().p_3().gap_3().child(
                    v_flex()
                        .gap_2()
                        .p_3()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Icon::new(IconName::Sparkles).text_color(theme.primary))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_semibold()
                                        .child("AI is off — set up an agent"),
                                ),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).child(
                            "Maze works fine without AI. Connect an agent to summarize \
                                 threads, draft replies and triage folders. Agents see mail \
                                 only through the mazemail command line, run in a sandbox, \
                                 and every change they propose waits for your review.",
                        ))
                        .child(setup("agent-claude", "Connect Claude Code"))
                        .child(setup("agent-codex", "Connect Codex"))
                        .child(setup("agent-local", "Use a local model")),
                ),
            )
            .when_some(self.context, |this, context| {
                this.child(
                    div()
                        .mx_3()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(SharedString::from(format!("Context: @thread {context}"))),
                )
            })
    }
}

/// The status bar: sync state, outbox, online state, Maze's disk usage and the palette
/// hint.
#[derive(IntoElement)]
pub(crate) struct MailStatusBar {
    status: SourceStatus,
    storage: Option<StorageUsage>,
    /// "Sync activity" behind the sync item: the source, whether it is open, and who
    /// to tell when that changes.
    activity: Option<(Arc<dyn MailSource>, bool, OpenChange)>,
}

impl MailStatusBar {
    /// A status bar for the source status and the disk usage (nothing shows for it
    /// until it is known).
    pub(crate) fn new(status: SourceStatus, storage: Option<StorageUsage>) -> Self {
        Self {
            status,
            storage,
            activity: None,
        }
    }

    /// Clicking the sync item opens "Sync activity" (see [`crate::sync_activity`]).
    pub(crate) fn sync_activity(
        mut self,
        source: Arc<dyn MailSource>,
        open: bool,
        on_open_change: OpenChange,
    ) -> Self {
        self.activity = Some((source, open, on_open_change));
        self
    }
}

/// The disk usage item's tooltip: the total, then what it is made of, then where.
pub(crate) fn storage_lines(usage: &StorageUsage) -> Vec<SharedString> {
    let mut lines: Vec<SharedString> = vec![
        format!(
            "{PRODUCT_NAME} uses {} of disk space",
            format_disk(usage.total())
        )
        .into(),
        format!(
            "Mail store: {} (database {}, messages {})",
            format_disk(usage.mail_store()),
            format_disk(usage.database),
            format_disk(usage.blobs)
        )
        .into(),
        format!("Search index: {}", format_disk(usage.search_index)).into(),
    ];
    for (label, bytes) in [
        ("Caches", usage.caches),
        ("Backups before updates", usage.backups),
        ("Logs and updates", usage.other),
    ] {
        if bytes > 0 {
            lines.push(format!("{label}: {}", format_disk(bytes)).into());
        }
    }
    lines.push(usage.data_dir.clone());
    lines
}

/// The status bar's disk usage, with the breakdown in the tooltip.
fn storage_item(usage: &StorageUsage) -> impl IntoElement + use<> {
    let lines = storage_lines(usage);
    h_flex()
        .id("status-storage")
        .debug_selector(|| "status-storage".to_owned())
        .gap_1()
        .items_center()
        .child(Icon::new(IconName::HardDrive).size_3())
        .child(format_disk(usage.total()))
        .tooltip(move |window, cx| {
            let lines = lines.clone();
            Tooltip::element(move |_, cx| {
                let muted = cx.theme().muted_foreground;
                v_flex()
                    .debug_selector(|| "status-storage-tooltip".to_owned())
                    .py_1()
                    .gap_0p5()
                    .children(lines.iter().enumerate().map(|(ix, line)| {
                        div()
                            .when(ix > 0, |this| this.text_color(muted))
                            .child(line.clone())
                    }))
            })
            .build(window, cx)
        })
}

impl RenderOnce for MailStatusBar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let sync: SharedString = match &self.status.sync {
            SyncState::Idle => "Up to date".into(),
            SyncState::Syncing {
                account,
                done,
                total,
            } => format!("{account}: syncing {done}/{total}").into(),
            SyncState::NoAccounts => "No accounts".into(),
            SyncState::Message { text, .. } => text.clone(),
        };
        let online_dot = div().size(px(7.)).rounded_full().bg(if self.status.online {
            theme.success
        } else {
            theme.muted_foreground
        });
        let sync_item = match self.activity {
            Some((source, open, on_open_change)) => {
                SyncActivityItem::new(source, sync, open, on_open_change).into_any_element()
            }
            None => StatusItem::new("status-sync", sync)
                .icon(IconName::RefreshCw)
                .into_any_element(),
        };
        StatusBar::new()
            .left(sync_item)
            .left(
                StatusItem::new(
                    "status-outbox",
                    SharedString::from(format!("Outbox {}", self.status.outbox)),
                )
                .icon(IconName::Send),
            )
            .left(
                h_flex()
                    .gap_1()
                    .child(online_dot)
                    .child(if self.status.online {
                        "Online"
                    } else {
                        "Offline"
                    }),
            )
            .left(StatusItem::new("status-agent", "AI off").icon(IconName::Bot))
            .when_some(self.storage.as_ref(), |bar, usage| {
                bar.right(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(storage_item(usage)),
                )
            })
            .right(
                Button::new("status-palette")
                    .ghost()
                    .xsmall()
                    .label("⌘⇧P commands")
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(Toggle), cx)),
            )
    }
}

/// How long the sync icon takes for one turn.
const SYNC_SPIN: Duration = Duration::from_millis(1_000);

/// The title bar's sync button, next to New Message: syncs every account. The icon
/// spins while a sync runs (or was just asked for), and a warning dot with the reason
/// in the tooltip shows a problem (offline, sign-in required, an error), the same state
/// the status bar shows.
#[derive(IntoElement)]
pub(crate) struct SyncButton {
    sync: SyncState,
    requested: bool,
}

impl SyncButton {
    /// The button for the sync state; `requested` while a sync just asked for hasn't
    /// shown up in the state yet.
    pub(crate) fn new(sync: SyncState, requested: bool) -> Self {
        Self { sync, requested }
    }

    /// Whether the icon spins, the problem to flag, and the tooltip.
    fn state(&self) -> (bool, Option<SharedString>, SharedString) {
        match &self.sync {
            SyncState::Syncing {
                account,
                done,
                total,
            } => (
                true,
                None,
                format!("Syncing… ({account}: {done}/{total})").into(),
            ),
            SyncState::Message {
                text,
                problem: false,
            } => (true, None, format!("Syncing… ({text})").into()),
            SyncState::Message {
                text,
                problem: true,
            } => (
                self.requested,
                Some(text.clone()),
                format!("Sync Now — {text}").into(),
            ),
            SyncState::NoAccounts => (false, None, "Sync Now — no accounts yet".into()),
            SyncState::Idle if self.requested => (true, None, "Syncing…".into()),
            SyncState::Idle => (false, None, "Sync Now".into()),
        }
    }
}

impl RenderOnce for SyncButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let (spinning, problem, tooltip) = self.state();
        let icon = Icon::new(IconName::RefreshCw).xsmall();
        let icon = if spinning {
            div()
                .debug_selector(|| "sync-now-spinning".to_owned())
                .flex()
                .child(icon.with_animation(
                    "sync-now-spin",
                    Animation::new(SYNC_SPIN).repeat(),
                    |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                ))
                .into_any_element()
        } else {
            icon.into_any_element()
        };
        div()
            .debug_selector(|| "sync-now".to_owned())
            .relative()
            .flex_none()
            .child(
                Button::new("sync-now")
                    .ghost()
                    .xsmall()
                    .size_5()
                    .accessibility_label("Sync Now")
                    .child(icon)
                    .tooltip_with_action(tooltip, &SyncNow, Some("Workspace"))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(SyncNow), cx)),
            )
            .when(problem.is_some(), |this| {
                this.child(
                    div()
                        .debug_selector(|| "sync-now-problem".to_owned())
                        .absolute()
                        .top(px(1.))
                        .right(px(1.))
                        .size(px(6.))
                        .rounded_full()
                        .bg(cx.theme().warning),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_button_follows_the_sync_state() {
        let state = |sync: SyncState, requested: bool| SyncButton::new(sync, requested).state();
        assert_eq!(
            state(SyncState::Idle, false),
            (false, None, "Sync Now".into())
        );
        assert_eq!(
            state(SyncState::Idle, true),
            (true, None, "Syncing…".into())
        );
        assert_eq!(
            state(
                SyncState::Syncing {
                    account: "Work".into(),
                    done: 3,
                    total: 10
                },
                false
            ),
            (true, None, "Syncing… (Work: 3/10)".into())
        );
        assert_eq!(
            state(
                SyncState::Message {
                    text: "Connecting…".into(),
                    problem: false
                },
                false
            ),
            (true, None, "Syncing… (Connecting…)".into())
        );
        let offline = "Work: Offline. Maze will try again in 30 seconds.";
        assert_eq!(
            state(
                SyncState::Message {
                    text: offline.into(),
                    problem: true
                },
                false
            ),
            (
                false,
                Some(offline.into()),
                format!("Sync Now — {offline}").into()
            )
        );
        assert_eq!(
            state(SyncState::NoAccounts, false),
            (false, None, "Sync Now — no accounts yet".into())
        );
    }
}

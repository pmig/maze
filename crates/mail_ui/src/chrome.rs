//! The window chrome around the mail columns: spaces bar, agent column and status bar.

use gpui::{
    App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    status_bar::StatusBar,
    v_flex,
};
use ui::assets::IconName;
use ui::components::StatusItem;

use crate::actions::{Toggle, ToggleAgentColumn};
use crate::source::{SourceStatus, SyncState};

/// The slim spaces bar on the far left (CONCEPT §8.2): Mail, Address Book, and at
/// the bottom Extensions and Settings. Only Mail exists so far.
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
                        space(
                            "space-settings",
                            IconName::Settings,
                            "Settings (coming later)",
                        )
                        .disabled(true),
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
                .tooltip("Agent setup arrives with the agents milestone (M6)")
        };
        column
            .child(
                h_flex()
                    .flex_none()
                    .h(px(36.))
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

/// The status bar: sync state, outbox, online state, and the palette hint.
#[derive(IntoElement)]
pub(crate) struct MailStatusBar {
    status: SourceStatus,
    summary: SharedString,
}

impl MailStatusBar {
    /// A status bar for the source status and a summary of the open list.
    pub(crate) fn new(status: SourceStatus, summary: SharedString) -> Self {
        Self { status, summary }
    }
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
            } => format!("{account} {done}/{total}").into(),
        };
        let online_dot = div().size(px(7.)).rounded_full().bg(if self.status.online {
            theme.success
        } else {
            theme.muted_foreground
        });
        StatusBar::new()
            .left(StatusItem::new("status-sync", sync).icon(IconName::RefreshCw))
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
            .right(StatusItem::new("status-summary", self.summary))
            .right(
                Button::new("status-palette")
                    .ghost()
                    .xsmall()
                    .label("⌘⇧P commands")
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(Toggle), cx)),
            )
    }
}

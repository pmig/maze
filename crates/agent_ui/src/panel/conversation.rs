//! The conversation's entries in the style of Zed's agent thread: the user's messages
//! as bordered bubbles, answers as plain Markdown, collapsible "Thinking" blocks,
//! tool calls as compact rows (icon, label, status, expand), terminal commands and
//! permission prompts as cards, the plan as a checklist card, errors as callouts.

use super::*;

/// The icon for a tool call's ACP kind.
fn tool_icon(kind: &str) -> IconName {
    match kind {
        "read" | "search" => IconName::Search,
        "edit" => IconName::FilePen,
        "delete" => IconName::Trash,
        "move" => IconName::ArrowRight,
        "execute" => IconName::SquareTerminal,
        "fetch" => IconName::Globe,
        "think" => IconName::Sparkles,
        _ => IconName::Settings2,
    }
}

fn chevron(expanded: bool) -> Icon {
    Icon::new(if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .xsmall()
}

impl AgentPanel {
    pub(super) fn entry(
        &self,
        ix: usize,
        entry: &Entry,
        session: &Session,
        cx: &mut Context<'_, Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let expanded = session.expanded.contains(&ix);
        let toggle = cx.listener(move |this, _: &ClickEvent, _, cx| {
            if let Some(session) = this.session.as_mut()
                && !session.expanded.remove(&ix)
            {
                session.expanded.insert(ix);
            }
            cx.notify();
        });
        match entry {
            Entry::User { text } => div()
                .debug_selector(move || format!("agent-user-message-{ix}"))
                .px_2()
                .pt_2()
                .pb_1()
                .child(
                    div()
                        .px_2()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .shadow_xs()
                        .text_sm()
                        .child(text.clone()),
                )
                .into_any_element(),
            Entry::Agent { text } => div()
                .debug_selector(move || format!("agent-answer-{ix}"))
                .px_4()
                .py_1()
                .text_sm()
                .child(TextView::markdown(("agent-answer", ix), text.clone()))
                .into_any_element(),
            Entry::Thought { text } => v_flex()
                .debug_selector(move || format!("agent-thinking-{ix}"))
                .px_4()
                .gap_1()
                .child(
                    h_flex()
                        .id(("agent-thought", ix))
                        .gap_1p5()
                        .cursor_pointer()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .on_click(toggle)
                        .child(Icon::new(IconName::Sparkles).xsmall())
                        .child("Thinking")
                        .child(chevron(expanded)),
                )
                .when(expanded, |this| {
                    this.child(
                        div()
                            .ml_1p5()
                            .pl_3()
                            .border_l_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(TextView::markdown(("agent-thought-text", ix), text.clone())),
                    )
                })
                .into_any_element(),
            Entry::ToolCall(call) => Self::tool_call(ix, call, expanded, toggle, cx),
            Entry::Plan { items } => Self::plan(items, cx),
            Entry::Terminal(terminal) => Self::terminal(ix, terminal, expanded, toggle, cx),
            Entry::Permission(prompt) => self.permission(prompt, cx),
            Entry::Notice { text, level } => match level {
                NoticeLevel::Error => h_flex()
                    .debug_selector(move || format!("agent-error-{ix}"))
                    .mx_2()
                    .p_2()
                    .gap_2()
                    .items_start()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.danger.opacity(0.4))
                    .bg(theme.danger.opacity(0.08))
                    .child(
                        Icon::new(IconName::CircleX)
                            .small()
                            .text_color(theme.danger),
                    )
                    .child(div().flex_1().min_w_0().text_xs().child(text.clone()))
                    .into_any_element(),
                NoticeLevel::Info => h_flex()
                    .px_4()
                    .gap_1p5()
                    .items_start()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::Info).xsmall())
                    .child(div().flex_1().min_w_0().child(text.clone()))
                    .into_any_element(),
            },
        }
    }

    fn tool_call(
        ix: usize,
        call: &ToolCallEntry,
        expanded: bool,
        toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let status: Option<AnyElement> = match call.status {
            ToolStatus::Pending | ToolStatus::Running => Some(
                Icon::new(IconName::LoaderCircle)
                    .xsmall()
                    .text_color(theme.muted_foreground)
                    .into_any_element(),
            ),
            ToolStatus::Failed => Some(
                Icon::new(IconName::CircleX)
                    .xsmall()
                    .text_color(theme.danger)
                    .into_any_element(),
            ),
            ToolStatus::Completed => None,
        };
        let has_detail = !call.detail.is_empty();
        v_flex()
            .debug_selector(move || format!("agent-tool-{ix}"))
            .px_4()
            .child(
                h_flex()
                    .id(("agent-tool", ix))
                    .gap_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when(has_detail, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(theme.muted))
                            .on_click(toggle)
                    })
                    .child(Icon::new(tool_icon(&call.kind)).xsmall())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(call.title.clone()),
                    )
                    .children(status)
                    .when(has_detail, |this| this.child(chevron(expanded))),
            )
            .when(expanded && has_detail, |this| {
                this.child(
                    div()
                        .ml_1p5()
                        .mt_1()
                        .pl_3()
                        .border_l_1()
                        .border_color(theme.border)
                        .text_xs()
                        .font_family("monospace")
                        .text_color(theme.muted_foreground)
                        .child(call.detail.clone()),
                )
            })
            .into_any_element()
    }

    fn plan(items: &[agent::PlanItem], cx: &App) -> AnyElement {
        let theme = cx.theme().colors;
        let done = items
            .iter()
            .filter(|i| i.status == PlanStatus::Completed)
            .count();
        let mut card = v_flex()
            .debug_selector(|| "agent-plan".to_owned())
            .mx_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .overflow_hidden()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .bg(theme.muted)
                    .text_xs()
                    .child(div().font_semibold().child("Plan"))
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(format!("{done} of {}", items.len()))),
                    ),
            );
        for item in items {
            let (icon, color) = match item.status {
                PlanStatus::Completed => (IconName::CircleCheck, theme.success),
                PlanStatus::InProgress => (IconName::LoaderCircle, theme.primary),
                PlanStatus::Pending => (IconName::CircleDot, theme.muted_foreground),
            };
            card = card.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_start()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .child(Icon::new(icon).xsmall().text_color(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .when(item.status == PlanStatus::Completed, |this| {
                                this.line_through().text_color(theme.muted_foreground)
                            })
                            .child(item.content.clone()),
                    ),
            );
        }
        card.into_any_element()
    }

    fn terminal(
        ix: usize,
        terminal: &TerminalEntry,
        expanded: bool,
        toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let (label, color): (SharedString, _) = match &terminal.status {
            TerminalStatus::AwaitingApproval => ("waiting for approval".into(), theme.warning),
            TerminalStatus::Running => ("running".into(), theme.muted_foreground),
            TerminalStatus::Exited { code: Some(0), .. } => ("done".into(), theme.success),
            TerminalStatus::Exited {
                code: Some(code), ..
            } => (format!("exited with code {code}").into(), theme.danger),
            TerminalStatus::Exited { signal, .. } => (
                signal.clone().unwrap_or_else(|| "ended".to_owned()).into(),
                theme.muted_foreground,
            ),
            TerminalStatus::Denied => ("denied".into(), theme.danger),
            TerminalStatus::Failed { message } => (message.clone().into(), theme.danger),
        };
        let running = terminal.status == TerminalStatus::Running;
        v_flex()
            .debug_selector(move || format!("agent-terminal-{ix}"))
            .mx_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .overflow_hidden()
            .child(
                h_flex()
                    .id(("agent-terminal", ix))
                    .px_2()
                    .py_1()
                    .gap_1p5()
                    .bg(theme.muted)
                    .cursor_pointer()
                    .on_click(toggle)
                    .text_xs()
                    .child(
                        Icon::new(IconName::SquareTerminal)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family("monospace")
                            .child(terminal.command.clone()),
                    )
                    .when(terminal.automatic, |this| {
                        this.child(div().text_color(theme.muted_foreground).child("auto"))
                    })
                    .when(running, |this| {
                        this.child(
                            Icon::new(IconName::LoaderCircle)
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        )
                    })
                    .child(div().text_color(color).child(label))
                    .child(chevron(expanded)),
            )
            .when(expanded && !terminal.output.is_empty(), |this| {
                this.child(
                    div()
                        .id(("agent-terminal-output", ix))
                        .max_h(scaled(280., cx))
                        .overflow_y_scroll()
                        .px_2()
                        .py_1p5()
                        .border_t_1()
                        .border_color(theme.border)
                        .text_xs()
                        .font_family("monospace")
                        .text_color(theme.muted_foreground)
                        .child(terminal.output.clone()),
                )
            })
            .into_any_element()
    }

    fn permission(&self, prompt: &PermissionEntry, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let pending = prompt.decision.is_none();
        let mut card = v_flex()
            .mx_2()
            .rounded_md()
            .border_1()
            .border_color(if pending { theme.warning } else { theme.border })
            .bg(theme.background)
            .overflow_hidden()
            .child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .gap_1p5()
                    .items_start()
                    .text_xs()
                    .child(
                        Icon::new(IconName::ShieldAlert)
                            .xsmall()
                            .text_color(if pending {
                                theme.warning
                            } else {
                                theme.muted_foreground
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_semibold()
                            .child(prompt.title.clone()),
                    ),
            );
        match prompt.decision {
            Some(choice) => {
                card = card.child(
                    div()
                        .px_2()
                        .pb_1p5()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(choice.label()),
                );
            }
            None => {
                let mut buttons = v_flex()
                    .p_1()
                    .gap_0p5()
                    .border_t_1()
                    .border_color(theme.border);
                for choice in &prompt.choices {
                    let id = prompt.id;
                    let choice = *choice;
                    let (icon, color, selector) = match choice {
                        PermissionChoice::AllowOnce => (
                            IconName::Check,
                            theme.success,
                            "agent-permission-allow-once",
                        ),
                        PermissionChoice::AllowSession => (
                            IconName::CircleCheck,
                            theme.success,
                            "agent-permission-allow-session",
                        ),
                        PermissionChoice::Deny => {
                            (IconName::Close, theme.danger, "agent-permission-deny")
                        }
                    };
                    let button = Button::new(SharedString::from(format!(
                        "agent-permission-{id}-{choice:?}"
                    )))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(icon).text_color(color))
                    .label(choice.label())
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.respond(id, choice, cx);
                        },
                    ));
                    buttons =
                        buttons.child(div().debug_selector(|| selector.to_owned()).child(button));
                }
                card = card.child(buttons);
            }
        }
        card.into_any_element()
    }
}

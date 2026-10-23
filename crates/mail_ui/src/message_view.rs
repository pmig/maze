//! The message view: header with sender, recipients, date and actions, the
//! verification row, attachments, and the body (CONCEPT §8.4).

use gpui::{
    App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, MouseButton,
    ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, button::Button, h_flex, text::TextView,
    v_flex,
};
use mazemail_types::Address;
use time::UtcOffset;
use ui::assets::IconName;

use crate::actions::{Archive, Forward, Reply, ReplyAll};
use crate::format::{DateFormatter, format_size, initials};
use crate::source::{MessageBody, MessageDocument, Verdict, Verification};

/// The message view.
pub struct MessageView {
    focus: FocusHandle,
    offset: UtcOffset,
    document: Option<MessageDocument>,
    error: Option<SharedString>,
}

impl std::fmt::Debug for MessageView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageView").finish_non_exhaustive()
    }
}

impl MessageView {
    /// An empty message view.
    pub fn new(offset: UtcOffset, cx: &mut Context<'_, Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            offset,
            document: None,
            error: None,
        }
    }

    /// Shows a message, or the empty state.
    pub fn show(&mut self, document: Option<MessageDocument>, cx: &mut Context<'_, Self>) {
        self.document = document;
        self.error = None;
        cx.notify();
    }

    /// Shows an error instead of a message.
    pub fn show_error(&mut self, error: impl Into<SharedString>, cx: &mut Context<'_, Self>) {
        self.document = None;
        self.error = Some(error.into());
        cx.notify();
    }

    /// The displayed message.
    pub fn document(&self) -> Option<&MessageDocument> {
        self.document.as_ref()
    }

    fn address_line(label: &'static str, addresses: &[Address], cx: &App) -> impl IntoElement {
        let text = addresses
            .iter()
            .map(|a| a.name.clone().unwrap_or_else(|| a.email.clone()))
            .collect::<Vec<_>>()
            .join(", ");
        h_flex()
            .gap_2()
            .text_xs()
            .child(
                div()
                    .w(px(36.))
                    .flex_none()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(div().truncate().child(text))
    }

    fn render_header(&self, doc: &MessageDocument, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dates = DateFormatter::new(self.offset);
        let from = doc.from.expose();
        let name: SharedString = from
            .name
            .clone()
            .unwrap_or_else(|| from.email.clone())
            .into();
        let action = |id: &'static str, label: &'static str, icon: IconName| {
            Button::new(id).outline().xsmall().icon(icon).label(label)
        };
        v_flex()
            .flex_none()
            .gap_2()
            .px_4()
            .pt_3()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .line_clamp(2)
                    .child(doc.subject.expose().clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .size(px(32.))
                            .flex_none()
                            .rounded_full()
                            .bg(theme.primary.opacity(0.2))
                            .text_color(theme.primary)
                            .text_xs()
                            .font_semibold()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(initials(&name)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_semibold().truncate().child(name))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .truncate()
                                    .child(from.email.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(dates.long(doc.date)),
                    ),
            )
            .child(Self::address_line("To", doc.to.expose(), cx))
            .when(!doc.cc.expose().is_empty(), |this| {
                this.child(Self::address_line("Cc", doc.cc.expose(), cx))
            })
            .child(Self::render_verification(&doc.verification, cx))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        action("reply", "Reply", IconName::Reply)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Reply), cx)),
                    )
                    .child(
                        action("reply-all", "All", IconName::ReplyAll).on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(ReplyAll), cx)
                        }),
                    )
                    .child(
                        action("forward", "Fwd", IconName::Forward).on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(Forward), cx)
                        }),
                    )
                    .child(
                        action("archive", "Archive", IconName::Archive).on_click(
                            |_, window, cx| window.dispatch_action(Box::new(Archive), cx),
                        ),
                    ),
            )
    }

    /// The verification row: DKIM, SPF and DMARC results and the verdict, with the
    /// explanation as a tooltip.
    fn render_verification(verification: &Verification, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let chip = |label: &'static str, result: SharedString| {
            let color = match result.as_ref() {
                "pass" => theme.success,
                "fail" => theme.danger,
                _ => theme.muted_foreground,
            };
            h_flex()
                .gap_0p5()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(theme.border)
                .child(div().text_color(theme.muted_foreground).child(label))
                .child(div().text_color(color).child(result))
        };
        let row = h_flex().id("verification").gap_1p5().text_xs();
        match verification {
            Verification::Pending | Verification::Unavailable => {
                let text = if *verification == Verification::Pending {
                    "Checking the sender…"
                } else {
                    "Sender can't be verified (no DNS)"
                };
                row.child(
                    Icon::new(IconName::ShieldCheck)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(chip("DKIM", "–".into()))
                .child(chip("SPF", "–".into()))
                .child(chip("DMARC", "–".into()))
                .child(div().text_color(theme.muted_foreground).child(text))
            }
            Verification::Checked {
                verdict,
                dkim,
                spf,
                dmarc,
                explanation,
            } => {
                let (icon, color, text) = match verdict {
                    Verdict::Verified => (IconName::ShieldCheck, theme.success, "Verified sender"),
                    Verdict::Unverified => (
                        IconName::ShieldCheck,
                        theme.muted_foreground,
                        "Not verified",
                    ),
                    Verdict::Suspicious => {
                        (IconName::ShieldAlert, theme.danger, "Suspicious sender")
                    }
                };
                let explanation = explanation.clone();
                row.child(Icon::new(icon).xsmall().text_color(color))
                    .child(chip("DKIM", dkim.clone()))
                    .child(chip("SPF", spf.clone()))
                    .child(chip("DMARC", dmarc.clone()))
                    .child(div().text_color(color).child(text))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(explanation.clone())
                            .build(window, cx)
                    })
            }
        }
    }

    fn render_attachments(doc: &MessageDocument, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_none()
            .flex_wrap()
            .gap_2()
            .px_4()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .children(doc.attachments.iter().map(|attachment| {
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .child(Icon::new(IconName::Paperclip).xsmall())
                    .child(attachment.name.expose().clone())
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(format_size(attachment.size)),
                    )
            }))
    }

    fn render_empty(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(Icon::new(IconName::Mail).large())
            .child(
                self.error
                    .clone()
                    .unwrap_or_else(|| SharedString::from("No message selected")),
            )
    }
}

impl Focusable for MessageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for MessageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let root = v_flex()
            .id("message-view")
            .size_full()
            .key_context("MessageView")
            .track_focus(&self.focus)
            // Clicking into the message gives it focus, so n/p/f/b keep working.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .bg(theme.background);
        let Some(doc) = self.document.clone() else {
            return root.child(self.render_empty(cx));
        };
        let body_id = SharedString::from(format!("message-body-{}", doc.message));
        root.child(self.render_header(&doc, cx))
            .when(!doc.attachments.is_empty(), |this| {
                this.child(Self::render_attachments(&doc, cx))
            })
            .child(
                div().flex_1().min_h_0().child(match &doc.body {
                    MessageBody::Html(html) => TextView::html(body_id, html.expose().clone())
                        .scrollable(true)
                        .selectable(true)
                        .size_full()
                        .px_4()
                        .py_3()
                        .text_sm()
                        .into_any_element(),
                    MessageBody::Markdown(markdown) => {
                        TextView::markdown(body_id, markdown.expose().clone())
                            .scrollable(true)
                            .selectable(true)
                            .size_full()
                            .px_4()
                            .py_3()
                            .text_sm()
                            .into_any_element()
                    }
                    MessageBody::PlainText(text) => div()
                        .id(body_id)
                        .size_full()
                        .overflow_y_scroll()
                        .px_4()
                        .py_3()
                        .text_sm()
                        .child(text.expose().clone())
                        .into_any_element(),
                }),
            )
    }
}

//! Maze Magic over the composer: `cmd-m` sends your text to an agent
//! ([`ComposeBackend::magic`]) and shows the improved text in a dialog over the
//! composer: Accept & Send (`enter`, the default), Accept & Edit (`cmd-e`) or Discard
//! (`escape`).
//!
//! Only the text above the quote ([`mazemail_mime::compose::quote_start`]) is replaced;
//! the quote or forwarded message below it stays as it is. Accepting is one undo step.
//! Closing the dialog while the agent works stops it.

use gpui::{
    AppContext as _, ClickEvent, Context, FocusHandle, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Task, Window, div, prelude::FluentBuilder as _, relative,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use mazemail_mime::compose::quote_start;
use mazemail_types::Sensitive;
use ui::assets::IconName;
use ui::keymap::shortcut_label;
use ui::theme::scaled;

use super::{Composer, Status};
use crate::actions::{AcceptAndEdit, AcceptAndSend, DiscardMagic};
use crate::backend::MagicInput;

/// Maze Magic's dialog.
pub(super) struct Magic {
    state: MagicState,
    focus: FocusHandle,
    /// The quote below your text, put back under the improved text.
    quoted: String,
}

enum MagicState {
    /// The agent is working; dropping the task stops it.
    Running(#[allow(dead_code)] Task<()>),
    /// The improved text.
    Ready(SharedString),
    /// Why there is none.
    Failed(SharedString),
}

/// Your text and the quote below it.
fn split(body: &str) -> (&str, &str) {
    let at = quote_start(body).unwrap_or(body.len());
    (&body[..at], &body[at..])
}

/// The body after accepting: the improved text, then the quote as it was.
fn joined(revised: &str, quoted: &str) -> String {
    let revised = revised.trim_end();
    if quoted.is_empty() {
        format!("{revised}\n")
    } else {
        format!("{revised}\n\n{quoted}")
    }
}

impl Composer {
    /// Starts Maze Magic on the draft's own text.
    pub(super) fn start_magic(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.sending || self.closed || self.magic.is_some() {
            return;
        }
        let body = self.body.read(cx).value().to_string();
        let (own, quoted) = split(&body);
        if own.trim().is_empty() {
            self.status =
                Status::Error("Write something first, then Maze Magic improves it.".into());
            cx.notify();
            return;
        }
        let input = MagicInput {
            identity: self.identity,
            origin: self.base.origin,
            draft: Sensitive::new(own.to_owned()),
            quoted: Sensitive::new(quoted.to_owned()),
        };
        let running = cx.background_spawn(self.backend.magic(input));
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = running.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(magic) = &mut this.magic {
                    magic.state = match result {
                        Ok(text) => MagicState::Ready(text.into_inner().into()),
                        Err(error) => MagicState::Failed(error.message.into()),
                    };
                    cx.notify();
                }
            });
        });
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        self.magic = Some(Magic {
            state: MagicState::Running(task),
            focus,
            quoted: quoted.to_owned(),
        });
        cx.notify();
    }

    /// Replaces your text with the improved one (one undo step), then sends or goes
    /// back to the body.
    pub(super) fn accept_magic(
        &mut self,
        send: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(magic) = &self.magic else {
            return;
        };
        let MagicState::Ready(revised) = &magic.state else {
            return;
        };
        let text = joined(revised, &magic.quoted);
        let (own, _) = split(&text);
        let caret = own.trim_end().len();
        self.magic = None;
        self.body.update(cx, |body, cx| {
            let all = 0..body.value().len();
            body.set_selected_range(all, cx);
            body.replace(text, window, cx);
            body.set_selected_range(caret..caret, cx);
        });
        if send {
            self.send(window, cx);
        } else {
            let body = self.body.read(cx).focus_handle(cx);
            window.focus(&body, cx);
        }
        cx.notify();
    }

    /// Closes the dialog (stopping the agent if it is still working); the draft stays
    /// as it was.
    pub(super) fn discard_magic(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.magic.take().is_some() {
            let body = self.body.read(cx).focus_handle(cx);
            window.focus(&body, cx);
            cx.notify();
        }
    }

    /// Maze Magic is working on the draft.
    pub fn magic_running(&self) -> bool {
        matches!(
            self.magic.as_ref().map(|m| &m.state),
            Some(MagicState::Running(_))
        )
    }

    /// The improved text waiting for a decision.
    pub fn magic_result(&self) -> Option<SharedString> {
        match self.magic.as_ref().map(|m| &m.state) {
            Some(MagicState::Ready(text)) => Some(text.clone()),
            _ => None,
        }
    }

    /// Why Maze Magic gave no text.
    pub fn magic_error(&self) -> Option<SharedString> {
        match self.magic.as_ref().map(|m| &m.state) {
            Some(MagicState::Failed(error)) => Some(error.clone()),
            _ => None,
        }
    }

    /// The dialog over the composer, while there is one.
    pub(super) fn render_magic(&self, cx: &mut Context<'_, Self>) -> Option<gpui::AnyElement> {
        let magic = self.magic.as_ref()?;
        let colors = cx.theme().colors;
        let (content, ready): (gpui::AnyElement, bool) = match &magic.state {
            MagicState::Running(_) => (
                v_flex()
                    .gap_1()
                    .p_4()
                    .text_sm()
                    .child("Improving your draft…")
                    .child(div().text_xs().text_color(colors.muted_foreground).child(
                        "The agent checks it against the conversation. This takes a moment.",
                    ))
                    .into_any_element(),
                false,
            ),
            MagicState::Ready(text) => (
                div()
                    .id("magic-result")
                    .debug_selector(|| "magic-result".to_owned())
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_4()
                    .text_sm()
                    .whitespace_normal()
                    .child(text.clone())
                    .into_any_element(),
                true,
            ),
            MagicState::Failed(error) => (
                div()
                    .p_4()
                    .text_sm()
                    .text_color(colors.danger)
                    .whitespace_normal()
                    .child(error.clone())
                    .into_any_element(),
                false,
            ),
        };
        let discard_label = if ready {
            "Discard"
        } else if self.magic_running() {
            "Cancel"
        } else {
            "Close"
        };
        let buttons = h_flex()
            .gap_2()
            .justify_end()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(colors.border)
            .child(
                Button::new("magic-discard")
                    .ghost()
                    .small()
                    .label(discard_label)
                    .tooltip(format!(
                        "{discard_label} ({})",
                        shortcut_label("escape", cx)
                    ))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.discard_magic(window, cx);
                    })),
            )
            .when(ready, |this| {
                this.child(
                    Button::new("magic-edit")
                        .small()
                        .label("Accept & Edit")
                        .tooltip(format!("Accept & Edit ({})", shortcut_label("cmd-e", cx)))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.accept_magic(false, window, cx);
                        })),
                )
                .child(
                    Button::new("magic-send")
                        .primary()
                        .small()
                        .label("Accept & Send")
                        .tooltip(format!("Accept & Send ({})", shortcut_label("enter", cx)))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.accept_magic(true, window, cx);
                        })),
                )
            });
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .bg(colors.overlay)
                .key_context("MazeMagic")
                .track_focus(&magic.focus)
                .on_action(cx.listener(|this, _: &AcceptAndSend, window, cx| {
                    this.accept_magic(true, window, cx);
                }))
                .on_action(cx.listener(|this, _: &AcceptAndEdit, window, cx| {
                    this.accept_magic(false, window, cx);
                }))
                .on_action(cx.listener(|this, _: &DiscardMagic, window, cx| {
                    this.discard_magic(window, cx);
                }))
                .child(
                    v_flex()
                        .id("maze-magic")
                        .w(scaled(560., cx))
                        .max_w_full()
                        .max_h(relative(0.9))
                        .rounded_lg()
                        .border_1()
                        .border_color(colors.border)
                        .bg(colors.popover)
                        .shadow_lg()
                        .child(
                            h_flex()
                                .gap_2()
                                .px_4()
                                .py_3()
                                .border_b_1()
                                .border_color(colors.border)
                                .text_sm()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child(Icon::new(IconName::Sparkles).small())
                                .child("Maze Magic"),
                        )
                        .child(content)
                        .child(buttons),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn your_text_and_the_quote_are_split_and_joined() {
        let body = "Hi Tom,\n\nthre months.\n\nOn Tue, Tom wrote:\n> Three or six?\n";
        let (own, quoted) = split(body);
        assert_eq!(own, "Hi Tom,\n\nthre months.\n\n");
        assert_eq!(quoted, "On Tue, Tom wrote:\n> Three or six?\n");
        assert_eq!(
            joined("Hi Tom,\n\nThree months.", quoted),
            "Hi Tom,\n\nThree months.\n\nOn Tue, Tom wrote:\n> Three or six?\n"
        );
        assert_eq!(split("Hello\n"), ("Hello\n", ""));
        assert_eq!(joined("Hello!\n\n", ""), "Hello!\n");
    }
}

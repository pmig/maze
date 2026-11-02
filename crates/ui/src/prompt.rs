//! Window prompts (`window.prompt`, "Save this message as a draft?") in Maze's theme.
//!
//! macOS shows native sheets. Elsewhere GPUI has no native prompt and falls back to a
//! renderer with a hard-coded white box, which stays light in a dark theme. [`init`]
//! replaces it there with this one: the theme's popover colors over the overlay, the
//! buttons in a row (the first one primary), `enter` for the first button and `escape`
//! for the cancel button.

use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, PromptButton, PromptHandle, PromptLevel,
    PromptResponse, Render, RenderablePromptHandle, SharedString, Styled as _, Window, div, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

/// Installs the themed prompt where the platform has no native one.
pub fn init(cx: &mut App) {
    if !cfg!(target_os = "macos") {
        cx.set_prompt_builder(themed_prompt);
    }
}

/// The prompt builder: a [`ThemedPrompt`] for one question.
pub fn themed_prompt(
    _level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    buttons: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let prompt = cx.new(|cx| ThemedPrompt {
        message: SharedString::from(message.to_owned()),
        detail: detail.map(|d| SharedString::from(d.to_owned())),
        buttons: buttons.iter().map(|b| b.label().clone()).collect(),
        cancel: cancel_index(buttons),
        focus: cx.focus_handle(),
    });
    handle.with_view(prompt, window, cx)
}

/// The button `escape` answers with: the cancel button, else the last one.
fn cancel_index(buttons: &[PromptButton]) -> Option<usize> {
    buttons
        .iter()
        .position(PromptButton::is_cancel)
        .or_else(|| buttons.len().checked_sub(1))
}

/// A modal question with a row of buttons.
pub struct ThemedPrompt {
    message: SharedString,
    detail: Option<SharedString>,
    buttons: Vec<SharedString>,
    cancel: Option<usize>,
    focus: FocusHandle,
}

impl std::fmt::Debug for ThemedPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThemedPrompt")
            .field("buttons", &self.buttons.len())
            .finish_non_exhaustive()
    }
}

impl ThemedPrompt {
    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<'_, Self>) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.modified() {
            return;
        }
        let answer = match keystroke.key.as_str() {
            "enter" => (!self.buttons.is_empty()).then_some(0),
            "escape" => self.cancel,
            _ => None,
        };
        if let Some(ix) = answer {
            cx.emit(PromptResponse(ix));
            cx.stop_propagation();
        }
    }
}

impl EventEmitter<PromptResponse> for ThemedPrompt {}

impl Focusable for ThemedPrompt {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ThemedPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let buttons = self.buttons.iter().enumerate().map(|(ix, label)| {
            let button = Button::new(("prompt-button", ix))
                .small()
                .label(label.clone())
                .on_click(cx.listener(move |_, _, _, cx| {
                    cx.emit(PromptResponse(ix));
                    cx.stop_propagation();
                }));
            if ix == 0 { button.primary() } else { button }
        });
        let dialog = v_flex()
            .id("prompt")
            .w(px(360.))
            .gap_2()
            .p_4()
            .occlude()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow_lg()
            .child(div().font_semibold().child(self.message.clone()))
            .children(self.detail.clone().map(|detail| {
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(detail)
            }))
            .child(h_flex().mt_2().gap_2().justify_end().children(buttons));
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.overlay)
            .child(dialog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_answers_with_the_cancel_button_or_the_last_one() {
        let save = [
            PromptButton::new("Save"),
            PromptButton::new("Don't Save"),
            PromptButton::cancel("Cancel"),
        ];
        assert_eq!(cancel_index(&save), Some(2));
        let others = [PromptButton::new("Open"), PromptButton::new("Copy Link")];
        assert_eq!(cancel_index(&others), Some(1));
        assert_eq!(cancel_index(&[]), None);
    }

    #[gpui::test]
    fn the_prompt_answers_enter_and_escape(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            cx.set_prompt_builder(themed_prompt);
        });
        let (_, cx) = cx.add_window_view(|_, _| gpui::Empty);
        for (keys, expected) in [("enter", 0), ("escape", 2)] {
            let mut answer = cx.update(|window, cx| {
                window.prompt(
                    PromptLevel::Warning,
                    "Save this message as a draft?",
                    None,
                    &["Save", "Don't Save", "Cancel"],
                    cx,
                )
            });
            cx.run_until_parked();
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            assert_eq!(answer.try_recv().unwrap(), Some(expected));
        }
    }
}

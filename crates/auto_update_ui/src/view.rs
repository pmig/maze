//! The title bar element: nothing when idle, a small progress indicator while
//! downloading, "Restart to update" once installed.

use gpui::{
    AnyElement, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    progress::ProgressCircle,
    spinner::Spinner,
    tooltip::Tooltip,
};
use ui::assets::IconName;

use crate::state::TitleBarState;
use crate::updater::AutoUpdater;

/// The updater's element in the title bar. Add it with
/// `MailWindow::add_title_bar_item`.
pub struct UpdateButton {
    updater: Entity<AutoUpdater>,
    _observe: Subscription,
}

impl std::fmt::Debug for UpdateButton {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateButton").finish_non_exhaustive()
    }
}

impl UpdateButton {
    /// A button following `updater`.
    pub fn new(updater: Entity<AutoUpdater>, cx: &mut Context<'_, Self>) -> Self {
        let observe = cx.observe(&updater, |_, _, cx| cx.notify());
        Self {
            updater,
            _observe: observe,
        }
    }
}

fn with_tooltip(id: &'static str, tooltip: Option<String>, child: AnyElement) -> AnyElement {
    let tooltip: SharedString = tooltip.unwrap_or_default().into();
    div()
        .id(id)
        .flex()
        .items_center()
        .child(child)
        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
        .into_any_element()
}

impl Render for UpdateButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let state = self.updater.read(cx).title_bar_state();
        let kind = state.kind();
        let label: SharedString = state.label().unwrap_or_default().into();
        let tooltip = state.tooltip();
        let updater = self.updater.clone();
        let muted = cx.theme().muted_foreground;
        let element = match &state {
            TitleBarState::Hidden => None,
            TitleBarState::Checking => Some(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(muted)
                    .child(Spinner::new().xsmall())
                    .child(label)
                    .into_any_element(),
            ),
            TitleBarState::Downloading { progress, .. } => Some(with_tooltip(
                "update-progress",
                tooltip,
                match progress {
                    Some(progress) => ProgressCircle::new("update-progress-circle")
                        .value(progress * 100.0)
                        .xsmall()
                        .into_any_element(),
                    None => ProgressCircle::new("update-progress-circle")
                        .loading(true)
                        .xsmall()
                        .into_any_element(),
                },
            )),
            TitleBarState::Installing { .. } => Some(with_tooltip(
                "update-installing",
                tooltip,
                Spinner::new().xsmall().into_any_element(),
            )),
            TitleBarState::Restart { .. } => Some(
                Button::new("restart-to-update")
                    .primary()
                    .xsmall()
                    .label(label)
                    .tooltip(tooltip.unwrap_or_default())
                    .on_click(move |_, window, cx| {
                        updater.update(cx, |updater, cx| updater.restart(window, cx));
                    })
                    .into_any_element(),
            ),
            TitleBarState::Available { .. } => Some(
                Button::new("update-available")
                    .outline()
                    .xsmall()
                    .icon(IconName::ArrowDown)
                    .label(label)
                    .tooltip(tooltip.unwrap_or_default())
                    .on_click(move |_, _, cx| {
                        updater.update(cx, |updater, cx| updater.install_available(cx));
                    })
                    .into_any_element(),
            ),
            TitleBarState::Error { .. } => Some(
                Button::new("update-error")
                    .ghost()
                    .xsmall()
                    .icon(IconName::TriangleAlert)
                    .tooltip(tooltip.unwrap_or_default())
                    .on_click(move |_, _, cx| {
                        updater.update(cx, |updater, cx| updater.dismiss(cx));
                    })
                    .into_any_element(),
            ),
        };
        div()
            .flex()
            .items_center()
            .debug_selector(move || format!("update-{kind}"))
            .children(element)
    }
}

//! Message rules in a tab ("Rules: Show Rules", ADR-029): every rule in the order it
//! runs, with its conditions and actions, what keeps it from running and a switch to
//! enable or disable it. Rules imported from Thunderbird start disabled; this is where
//! they are reviewed and turned on.
//!
//! Creating and editing rules happens with `mazemail rule add` for now; the tab only
//! lists and toggles them. The tab is kept in the layout (`{"kind": "rules"}`).

use std::sync::Arc;

use gpui::{
    App, ClickEvent, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, prelude::FluentBuilder as _,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use mazemail_types::{AccountId, Sensitive};
use ui::assets::IconName;
use ui::panes::{Item, ItemEvent, SerializedItem};

use crate::source::{MailSource, SourceError};

/// The layout kind of the rules tab.
pub const RULES_KIND: &str = "rules";

/// One rule, as the tab shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct RuleEntry {
    /// The backend's id.
    pub id: i64,
    /// The rule's name.
    pub name: SharedString,
    /// Its account, `None` for every account.
    pub account: Option<AccountId>,
    /// The account's name ("All accounts" for `None`).
    pub account_name: SharedString,
    /// Whether it runs on new mail.
    pub enabled: bool,
    /// The conditions, readable. Holds addresses: never logged.
    pub conditions: Sensitive<SharedString>,
    /// The actions, readable.
    pub actions: SharedString,
    /// What keeps it from running as intended.
    pub problems: Vec<SharedString>,
    /// Whether it may be enabled.
    pub runnable: bool,
    /// Imported from Thunderbird.
    pub imported: bool,
}

impl std::fmt::Debug for RuleEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuleEntry")
            .field("id", &self.id)
            .field("enabled", &self.enabled)
            .field("runnable", &self.runnable)
            .finish_non_exhaustive()
    }
}

/// What "Run Rules on Folder" did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RulesRun {
    /// Messages looked at.
    pub examined: usize,
    /// Messages a rule acted on.
    pub matched: usize,
}

/// The rules tab.
pub struct RulesTab {
    source: Arc<dyn MailSource>,
    rules: Vec<RuleEntry>,
    error: Option<SharedString>,
    focus: FocusHandle,
}

impl std::fmt::Debug for RulesTab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RulesTab")
            .field("rules", &self.rules.len())
            .finish_non_exhaustive()
    }
}

impl RulesTab {
    /// A tab over `source`'s rules.
    pub fn new(source: Arc<dyn MailSource>, cx: &mut Context<'_, Self>) -> Self {
        let mut tab = Self {
            source,
            rules: Vec::new(),
            error: None,
            focus: cx.focus_handle(),
        };
        tab.reload(cx);
        tab
    }

    /// The rules shown.
    pub fn rules(&self) -> &[RuleEntry] {
        &self.rules
    }

    /// The last error, if a toggle failed.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// Reads the rules again.
    pub fn reload(&mut self, cx: &mut Context<'_, Self>) {
        self.rules = self.source.rules();
        cx.notify();
    }

    /// Enables or disables a rule.
    pub fn set_enabled(&mut self, id: i64, enabled: bool, cx: &mut Context<'_, Self>) {
        self.error = match self.source.set_rule_enabled(id, enabled) {
            Ok(()) => None,
            Err(error) => Some(error.to_string().into()),
        };
        self.reload(cx);
    }

    fn render_rule(
        &self,
        index: usize,
        rule: &RuleEntry,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let colors = cx.theme().colors;
        let id = rule.id;
        let enabled = rule.enabled;
        let toggle = Button::new(("rule-toggle", index))
            .debug_selector(move || format!("rule-toggle-{id}"))
            .small()
            .when(enabled, |b| b.ghost())
            .when(!enabled, |b| b.primary())
            .label(if enabled { "Disable" } else { "Enable" })
            .disabled(!enabled && !rule.runnable)
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.set_enabled(id, !enabled, cx);
            }));
        let state = match (rule.enabled, rule.imported) {
            (true, _) => "On",
            (false, true) => "Off (imported from Thunderbird)",
            (false, false) => "Off",
        };
        v_flex()
            .gap_1()
            .px_4()
            .py_3()
            .border_b_1()
            .border_color(colors.border)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(rule.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(colors.muted_foreground)
                            .child(format!("{} · {state}", rule.account_name)),
                    )
                    .child(div().ml_auto().child(toggle)),
            )
            .child(
                div()
                    .text_sm()
                    .child(format!("When {}", rule.conditions.expose())),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .child(format!("Then {}", rule.actions)),
            )
            .children(rule.problems.iter().map(|problem| {
                div()
                    .text_xs()
                    .text_color(colors.warning)
                    .child(problem.clone())
            }))
    }
}

impl EventEmitter<ItemEvent> for RulesTab {}

impl Focusable for RulesTab {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for RulesTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let rows: Vec<_> = self
            .rules
            .clone()
            .iter()
            .enumerate()
            .map(|(index, rule)| self.render_rule(index, rule, cx).into_any_element())
            .collect();
        v_flex()
            .id("rules-tab")
            .debug_selector(|| "rules-tab".to_owned())
            .track_focus(&self.focus)
            .key_context("RulesTab")
            .size_full()
            .overflow_y_scroll()
            .bg(colors.background)
            .text_color(colors.foreground)
            .child(
                div()
                    .px_4()
                    .py_3()
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .child(
                        "Rules run in this order on new mail in the Inbox. Add rules with \
                         `mazemail rule add`; right-click a folder and choose Run Rules on \
                         Folder to sort the mail that is already there.",
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_4()
                        .pb_2()
                        .text_sm()
                        .text_color(colors.danger)
                        .child(error),
                )
            })
            .when(rows.is_empty(), |this| {
                this.child(
                    div()
                        .px_4()
                        .py_3()
                        .child("No rules yet. Filters imported from Thunderbird show up here."),
                )
            })
            .children(rows)
    }
}

impl Item for RulesTab {
    fn tab_title(&self, _: &App) -> SharedString {
        "Rules".into()
    }

    fn tab_icon(&self, _: &App) -> Option<IconName> {
        Some(IconName::ListFilter)
    }

    fn serialize(&self, _: &App) -> Option<SerializedItem> {
        Some(SerializedItem::new(RULES_KIND, serde_json::json!({})))
    }
}

/// The error for backends without rules.
pub(crate) fn no_rules() -> SourceError {
    SourceError::Unavailable("rules aren't available".into())
}

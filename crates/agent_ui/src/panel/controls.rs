//! The model, mode, thinking and other pickers in the prompt's bottom bar (Zed's
//! message editor), and their keys: `shift-tab` cycles the mode, `alt-tab` the model,
//! `ctrl-'` the thinking effort.

use agent::controls::by_category;
use agent::{Control, ControlCategory, ControlKind, ControlValue};
use gpui::WeakEntity;
use gpui_kit::component::Selectable as _;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};

use super::*;

/// The debug selector of a control's button.
pub(crate) fn control_selector(control: &Control) -> String {
    match control.category {
        ControlCategory::Mode => "agent-control-mode".to_owned(),
        ControlCategory::Model => "agent-control-model".to_owned(),
        ControlCategory::ThoughtLevel => "agent-control-thinking".to_owned(),
        ControlCategory::ModelConfig | ControlCategory::Other => {
            format!("agent-control-{}", control.id)
        }
    }
}

impl AgentPanel {
    /// The running session's controls.
    pub fn controls(&self) -> &[Control] {
        self.session.as_ref().map_or(&[], |s| &s.controls)
    }

    /// Changes a control of the running session and remembers the choice for the
    /// agent's next sessions.
    pub fn set_control(&mut self, id: &str, value: &ControlValue, cx: &mut Context<'_, Self>) {
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if session.status != SessionStatus::Ready {
            return;
        }
        let Some(control) = session.controls.iter_mut().find(|c| c.id == id) else {
            return;
        };
        if !control.accepts(value) || control.value() == *value {
            return;
        }
        // Shown at once; the agent's answer (or its refusal) follows.
        control.set(value);
        session.handle.set_control(id, value.clone());
        if let Err(error) = self.manager.remember_control(&session.agent_id, id, value) {
            tracing::warn!(%error, "the agent choice couldn't be remembered");
        }
        cx.notify();
    }

    /// Moves the first control of `category` to its next value.
    pub(super) fn cycle_control(&mut self, category: ControlCategory, cx: &mut Context<'_, Self>) {
        let Some((id, next)) = self.session.as_ref().and_then(|session| {
            let control = by_category(&session.controls, category)?;
            Some((control.id.clone(), control.next()?))
        }) else {
            return;
        };
        self.set_control(&id, &next, cx);
    }

    /// The pickers: mode on the left, the rest on the right (model, thinking, others).
    pub(super) fn control_bar(
        &self,
        session: &Session,
        cx: &mut Context<'_, Self>,
    ) -> (Vec<AnyElement>, Vec<AnyElement>) {
        let enabled = session.status == SessionStatus::Ready && session.ended.is_none();
        let panel = cx.entity().downgrade();
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut placed: Vec<&str> = Vec::new();
        for category in [
            ControlCategory::Mode,
            ControlCategory::Model,
            ControlCategory::ThoughtLevel,
        ] {
            if let Some(control) = by_category(&session.controls, category) {
                placed.push(control.id.as_str());
                let button = Self::control_button(control, enabled, &panel, cx);
                if category == ControlCategory::Mode {
                    left.push(button);
                } else {
                    right.push(button);
                }
            }
        }
        for control in &session.controls {
            if !placed.contains(&control.id.as_str()) {
                right.push(Self::control_button(control, enabled, &panel, cx));
            }
        }
        (left, right)
    }

    fn control_button(
        control: &Control,
        enabled: bool,
        panel: &WeakEntity<Self>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let selector = control_selector(control);
        let id = SharedString::from(selector.clone());
        let hint = match control.category {
            ControlCategory::Mode => " (⇧⇥)",
            ControlCategory::Model => " (⌥⇥)",
            ControlCategory::ThoughtLevel => " (⌃')",
            _ => "",
        };
        let tooltip: SharedString = match &control.description {
            Some(description) => format!("{}{hint}\n{description}", control.name).into(),
            None => format!("{}{hint}", control.name).into(),
        };
        let button = match &control.kind {
            ControlKind::Toggle(on) => {
                let on = *on;
                let control_id = control.id.clone();
                let panel = panel.clone();
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .label(control.name.clone())
                    .selected(on)
                    .disabled(!enabled)
                    .tooltip(tooltip)
                    .on_click(move |_, _, cx| {
                        panel
                            .update(cx, |panel, cx| {
                                panel.set_control(&control_id, &ControlValue::Toggle(!on), cx);
                            })
                            .ok();
                    })
                    .into_any_element()
            }
            ControlKind::Select { current, choices } => {
                let control_id = control.id.clone();
                let name = control.name.clone();
                let current = current.clone();
                let choices = choices.clone();
                let panel = panel.clone();
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .label(truncate(&control.current_name(), 28))
                    .dropdown_caret(true)
                    .disabled(!enabled)
                    .tooltip(tooltip)
                    .dropdown_menu(move |menu: PopupMenu, _, cx| {
                        let mut menu = menu
                            .label(name.clone())
                            .max_h(scaled(320., cx))
                            .scrollable(true);
                        let mut group: Option<&String> = None;
                        for choice in &choices {
                            if choice.group.is_some() && choice.group.as_ref() != group {
                                group = choice.group.as_ref();
                                if let Some(group) = group {
                                    menu = menu.label(group.clone());
                                }
                            }
                            let panel = panel.clone();
                            let control_id = control_id.clone();
                            let value = choice.value.clone();
                            menu = menu.item(
                                PopupMenuItem::new(choice.name.clone())
                                    .checked(choice.value == current)
                                    .on_click(move |_, _, cx| {
                                        panel
                                            .update(cx, |panel, cx| {
                                                panel.set_control(
                                                    &control_id,
                                                    &ControlValue::Choice(value.clone()),
                                                    cx,
                                                );
                                            })
                                            .ok();
                                    }),
                            );
                        }
                        menu
                    })
                    .into_any_element()
            }
        };
        div()
            .debug_selector(move || selector)
            .text_color(theme.muted_foreground)
            .child(button)
            .into_any_element()
    }
}

/// `text` cut to `max` characters with an ellipsis.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

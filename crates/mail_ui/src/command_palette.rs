//! The command palette (`cmd-shift-p`, CONCEPT §8.3): every registered Maze action
//! with its key binding, filtered as you type.
//!
//! It renders inside the main window's element tree (not as a Root dialog), so the
//! action it dispatches bubbles from the palette to the window's handlers.
//!
//! Key bindings are looked up where the focus was before the palette opened, as in
//! Zed: from the palette's own focus, bindings with a context such as `ThreadList`
//! (`a` for Archive) wouldn't match, and the hint would be missing.

use gpui::{
    Action, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_kit::component::{
    ActiveTheme as _,
    command::{Command, CommandItem, CommandState},
    h_flex,
    kbd::Kbd,
};

use crate::actions::{PALETTE_NAMESPACES, humanize_action_name};

/// Emitted when the palette should close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dismissed;

/// One palette entry: a label and the action name it dispatches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    /// "Mail: Next Unread".
    pub label: String,
    /// "mail::NextUnread".
    pub action: &'static str,
}

/// The palette entries for a list of registered action names, sorted by label.
pub fn palette_entries<'a>(names: impl IntoIterator<Item = &'a &'static str>) -> Vec<PaletteEntry> {
    let mut entries: Vec<_> = names
        .into_iter()
        .filter(|name| {
            name.split_once("::")
                .is_some_and(|(namespace, _)| PALETTE_NAMESPACES.contains(&namespace))
        })
        .map(|name| PaletteEntry {
            label: humanize_action_name(name),
            action: name,
        })
        .collect();
    entries.sort_by(|a, b| a.label.cmp(&b.label));
    entries
}

/// The palette view.
pub struct CommandPalette {
    state: Entity<CommandState>,
    items: Vec<CommandItem>,
}

impl std::fmt::Debug for CommandPalette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandPalette").finish_non_exhaustive()
    }
}

impl CommandPalette {
    /// A palette over every registered Maze action. `return_focus` is where the focus
    /// was before the palette opened; the key binding hints are looked up there.
    pub fn new(
        return_focus: Option<&FocusHandle>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let items = palette_entries(cx.all_action_names())
            .into_iter()
            .filter_map(|entry| {
                let action = cx.build_action(entry.action, None).ok()?;
                let item = CommandItem::new()
                    .label(entry.label.clone())
                    .keywords([entry.action]);
                Some(match binding_hint(action.as_ref(), return_focus, window) {
                    // The command list skips its own lookup (from the palette's focus)
                    // for rows with custom content, so the hint isn't shown twice.
                    Some(keystroke) => item
                        .child(move |_, _| {
                            h_flex()
                                .flex_1()
                                .gap_2()
                                .items_center()
                                .child(entry.label.clone())
                                .child(Kbd::new(keystroke.clone()).ml_auto())
                        })
                        .action(action),
                    None => item.action(action),
                })
            })
            .collect();
        Self {
            state: cx.new(|cx| CommandState::new(window, cx)),
            items,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no entries.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The first keystroke of the action's binding where `focus` is, else of a binding
/// that applies anywhere.
pub(crate) fn binding_hint(
    action: &dyn Action,
    focus: Option<&FocusHandle>,
    window: &Window,
) -> Option<gpui::Keystroke> {
    let binding = focus
        .and_then(|focus| window.highest_precedence_binding_for_action_in(action, focus))
        .or_else(|| {
            window.highest_precedence_binding_for_action_in_context(
                action,
                gpui::KeyContext::default(),
            )
        })?;
    Some(binding.keystrokes().first()?.inner().clone())
}

impl EventEmitter<Dismissed> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.state.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let this = cx.weak_entity();
        let cancel = this.clone();
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .justify_center()
            .pt(px(72.))
            .bg(theme.overlay)
            .on_mouse_down(gpui::MouseButton::Left, {
                let this = this.clone();
                move |_, _, cx| {
                    this.update(cx, |_, cx| cx.emit(Dismissed)).ok();
                }
            })
            .child(
                div()
                    .w(px(560.))
                    .h(px(380.))
                    .occlude()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .shadow_lg()
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Command::new(&self.state)
                            .bordered(false)
                            .placeholder("Run a command…")
                            .max_h(px(330.))
                            .items(self.items.iter().cloned())
                            .on_confirm(move |_, _, cx| {
                                this.update(cx, |_, cx| cx.emit(Dismissed)).ok();
                            })
                            .on_cancel(move |_, cx| {
                                cancel.update(cx, |_, cx| cx.emit(Dismissed)).ok();
                            }),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_maze_actions_only() {
        let names: Vec<&'static str> = vec![
            "mail::NextUnread",
            "input::Backspace",
            "workspace::ToggleFolderPane",
            "command_palette::Toggle",
            "maze::Quit",
        ];
        let entries = palette_entries(&names);
        let labels: Vec<_> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Mail: Next Unread",
                "Maze: Quit",
                "Workspace: Toggle Folder Pane"
            ]
        );
    }
}

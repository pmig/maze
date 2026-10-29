//! Keymaps in Zed's JSON format, turned into GPUI key bindings.
//!
//! A keymap is a list of sections. Each section has an optional context predicate
//! (the same language as Zed: `"ThreadList"`, `"Workspace && !Input"`, …) and a map
//! from keystrokes to actions:
//!
//! ```json
//! [
//!   { "context": "ThreadList", "bindings": { "a": "mail::Archive", "n": "mail::NextUnread" } },
//!   { "bindings": { "cmd-shift-p": "command_palette::Toggle" } }
//! ]
//! ```
//!
//! An action is written as its name (`"mail::Archive"`), as `[name, arguments]` for
//! actions that take data, or as `null` to unbind the keystroke. Comments and
//! trailing commas are allowed, as in Zed's keymap files.
//!
//! Loading is forgiving: a binding that names an unknown action or an invalid
//! keystroke is reported in [`LoadedKeymap::errors`] and skipped, and the rest of the
//! keymap still loads.
//!
//! # The primary modifier
//!
//! Keymaps write the main modifier as `cmd`. [`set_primary_modifier`] decides which key
//! that is when bindings are built ([`translate_keystrokes`]): the Command / Super key
//! ([`PrimaryModifier::Super`], the default and what the file says), or Ctrl
//! ([`PrimaryModifier::Ctrl`], the usual choice on Linux). Maze picks it from the
//! `keymap.primary_modifier` setting before it binds the default keymap.
//! [`shortcut_label`] writes a shortcut for people with the same choice, as macOS
//! symbols (`⌘⇧F`) or as text elsewhere (`Ctrl+Shift+F`).

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use gpui::{Action, App, Global, KeyBinding, KeyBindingContextPredicate, NoAction, SharedString};
use serde::Deserialize;

/// Maze's default macOS keymap (Thunderbird bindings plus Zed's, CONCEPT §12).
pub const DEFAULT_MACOS_KEYMAP: &str = include_str!("../../../assets/keymaps/default-macos.json");

/// Which key a keymap's `cmd` stands for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum PrimaryModifier {
    /// Command on macOS, the Super (Windows) key elsewhere: the keymap as written.
    #[default]
    Super,
    /// Ctrl.
    Ctrl,
}

impl PrimaryModifier {
    /// The platform's convention: Command on macOS, Ctrl elsewhere.
    pub const fn for_platform() -> Self {
        if cfg!(target_os = "macos") {
            Self::Super
        } else {
            Self::Ctrl
        }
    }
}

/// The primary modifier bindings are built with.
struct ActivePrimaryModifier(PrimaryModifier);

impl Global for ActivePrimaryModifier {}

/// Sets which key `cmd` stands for in keymaps bound from now on, and in
/// [`shortcut_label`]. Bindings made before keep their keys, so call this before
/// binding the default keymap.
pub fn set_primary_modifier(modifier: PrimaryModifier, cx: &mut App) {
    cx.set_global(ActivePrimaryModifier(modifier));
}

/// The primary modifier in use ([`PrimaryModifier::Super`] unless set).
pub fn primary_modifier(cx: &App) -> PrimaryModifier {
    cx.try_global::<ActivePrimaryModifier>()
        .map(|active| active.0)
        .unwrap_or_default()
}

const MODIFIERS: &[&str] = &[
    "cmd",
    "super",
    "win",
    "ctrl",
    "control",
    "alt",
    "option",
    "shift",
    "fn",
    "secondary",
];

/// Splits one keystroke (`cmd-shift-f`) into its modifiers and its key. The key may
/// itself be `-` (`cmd--`).
fn split_keystroke(stroke: &str) -> (Vec<&str>, &str) {
    let mut modifiers = Vec::new();
    let mut rest = stroke;
    while let Some((head, tail)) = rest.split_once('-')
        && !tail.is_empty()
        && MODIFIERS.iter().any(|m| m.eq_ignore_ascii_case(head))
    {
        modifiers.push(head);
        rest = tail;
    }
    (modifiers, rest)
}

/// `keystrokes` (space-separated) with `cmd` turned into `modifier`. With
/// [`PrimaryModifier::Ctrl`], a keystroke that has both `cmd` and `ctrl`
/// (`cmd-ctrl-left`) keeps two modifiers: `cmd` becomes Ctrl and `ctrl` becomes Super.
pub fn translate_keystrokes(keystrokes: &str, modifier: PrimaryModifier) -> String {
    if modifier == PrimaryModifier::Super {
        return keystrokes.to_owned();
    }
    keystrokes
        .split_whitespace()
        .map(|stroke| {
            let (modifiers, key) = split_keystroke(stroke);
            let is = |m: &str, name: &str| m.eq_ignore_ascii_case(name);
            if !modifiers.iter().any(|m| is(m, "cmd")) {
                return stroke.to_owned();
            }
            let both = modifiers.iter().any(|m| is(m, "ctrl") || is(m, "control"));
            let mut out: Vec<&str> = modifiers
                .iter()
                .map(|m| {
                    if is(m, "cmd") {
                        "ctrl"
                    } else if both && (is(m, "ctrl") || is(m, "control")) {
                        "super"
                    } else {
                        m
                    }
                })
                .collect();
            out.push(key);
            out.join("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A shortcut from a keymap (`cmd-shift-f`) as people read it, with the primary
/// modifier in use: `⌘⇧F` on macOS, `Ctrl+Shift+F` elsewhere.
pub fn shortcut_label(keystrokes: &str, cx: &App) -> String {
    format_shortcut(
        &translate_keystrokes(keystrokes, primary_modifier(cx)),
        cfg!(target_os = "macos"),
    )
}

/// [`shortcut_label`] for already translated keystrokes, as macOS symbols or as text.
fn format_shortcut(keystrokes: &str, symbols: bool) -> String {
    keystrokes
        .split_whitespace()
        .map(|stroke| {
            let (modifiers, key) = split_keystroke(stroke);
            let has = |names: &[&str]| {
                modifiers
                    .iter()
                    .any(|m| names.iter().any(|n| m.eq_ignore_ascii_case(n)))
            };
            // macOS order: ⌃⌥⇧⌘.
            let order: [(&[&str], &str, &str); 4] = [
                (&["ctrl", "control"], "⌃", "Ctrl"),
                (&["alt", "option"], "⌥", "Alt"),
                (&["shift"], "⇧", "Shift"),
                (&["cmd", "super", "win"], "⌘", "Super"),
            ];
            let names: Vec<&str> = order
                .iter()
                .filter(|(names, _, _)| has(names))
                .map(|(_, symbol, text)| if symbols { *symbol } else { *text })
                .collect();
            let key = key_label(key, symbols);
            if symbols {
                format!("{}{key}", names.concat())
            } else {
                names
                    .into_iter()
                    .chain(std::iter::once(key.as_str()))
                    .collect::<Vec<_>>()
                    .join("+")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A key's name for people.
fn key_label(key: &str, symbols: bool) -> String {
    let named = match key {
        "enter" => Some(("↩", "Enter")),
        "escape" => Some(("⎋", "Esc")),
        "backspace" => Some(("⌫", "Backspace")),
        "delete" => Some(("⌦", "Delete")),
        "tab" => Some(("⇥", "Tab")),
        "space" => Some(("␣", "Space")),
        "left" => Some(("←", "Left")),
        "right" => Some(("→", "Right")),
        "up" => Some(("↑", "Up")),
        "down" => Some(("↓", "Down")),
        _ => None,
    };
    match named {
        Some((symbol, text)) => (if symbols { symbol } else { text }).to_owned(),
        None if key.chars().count() == 1 => key.to_uppercase(),
        None => {
            let mut chars = key.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    }
}

/// A keymap file that couldn't be read at all.
#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    /// Not valid keymap JSON.
    #[error("invalid keymap file: {0}")]
    Parse(#[from] serde_json::Error),
}

/// A parsed keymap file: sections in file order. Later sections win over earlier
/// ones for the same keystroke and context depth, as in Zed.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct KeymapFile(pub Vec<KeymapSection>);

/// One `{ "context": …, "bindings": … }` block.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct KeymapSection {
    /// The context predicate; `None` binds everywhere.
    #[serde(default)]
    pub context: Option<String>,
    /// Match keys by their position on a US layout instead of the character they
    /// produce (macOS key equivalents).
    #[serde(default)]
    pub use_key_equivalents: bool,
    /// Keystrokes (space-separated for sequences) to actions.
    #[serde(default)]
    pub bindings: BTreeMap<String, KeymapAction>,
}

/// The right-hand side of a binding.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum KeymapAction {
    /// `null`: unbinds the keystroke in this context.
    Unbind,
    /// `"namespace::Action"`.
    Name(String),
    /// `["namespace::Action", { …arguments… }]`.
    WithArguments(String, serde_json::Value),
}

impl KeymapAction {
    /// The action name, if this isn't an unbind.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Unbind => None,
            Self::Name(name) | Self::WithArguments(name, _) => Some(name),
        }
    }
}

/// Why one binding was skipped.
#[derive(Debug, thiserror::Error)]
pub enum BindingErrorKind {
    /// No action with that name is registered.
    #[error("unknown action `{0}`")]
    UnknownAction(String),
    /// The action exists but rejected its arguments.
    #[error("invalid arguments for `{name}`: {message}")]
    InvalidArguments {
        /// The action name.
        name: String,
        /// What was wrong.
        message: String,
    },
    /// The keystrokes couldn't be parsed.
    #[error("invalid keystroke `{0}`")]
    InvalidKeystroke(String),
    /// The context predicate couldn't be parsed.
    #[error("invalid context `{context}`: {message}")]
    InvalidContext {
        /// The predicate as written.
        context: String,
        /// What was wrong.
        message: String,
    },
}

/// A binding that couldn't be loaded, with where it came from.
#[derive(Debug, thiserror::Error)]
#[error("{keystrokes} in {}: {kind}", context.as_deref().unwrap_or("any context"))]
pub struct BindingError {
    /// The section's context predicate.
    pub context: Option<String>,
    /// The keystrokes as written.
    pub keystrokes: String,
    /// What went wrong.
    pub kind: BindingErrorKind,
}

/// The bindings built from a keymap, plus the ones that were skipped.
#[derive(Default)]
pub struct LoadedKeymap {
    /// Ready to pass to [`App::bind_keys`].
    pub bindings: Vec<KeyBinding>,
    /// Bindings that were skipped.
    pub errors: Vec<BindingError>,
}

impl fmt::Debug for LoadedKeymap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoadedKeymap")
            .field("bindings", &self.bindings.len())
            .field("errors", &self.errors)
            .finish()
    }
}

impl KeymapFile {
    /// Parses keymap JSON. Comments and trailing commas are allowed.
    pub fn parse(source: &str) -> Result<Self, KeymapError> {
        Ok(serde_json::from_str(&strip_jsonc(source))?)
    }

    /// Builds GPUI key bindings, with `cmd` as the [primary modifier](primary_modifier)
    /// in use. Actions must be registered (any `actions!` type linked into the binary
    /// is) before this runs.
    pub fn load(&self, cx: &App) -> LoadedKeymap {
        let modifier = primary_modifier(cx);
        let mut loaded = LoadedKeymap::default();
        for section in &self.0 {
            let predicate = match section.context.as_deref().map(str::trim) {
                None | Some("") => None,
                Some(context) => match KeyBindingContextPredicate::parse(context) {
                    Ok(predicate) => Some(Rc::new(predicate)),
                    Err(error) => {
                        for keystrokes in section.bindings.keys() {
                            loaded.errors.push(BindingError {
                                context: section.context.clone(),
                                keystrokes: keystrokes.clone(),
                                kind: BindingErrorKind::InvalidContext {
                                    context: context.to_owned(),
                                    message: error.to_string(),
                                },
                            });
                        }
                        continue;
                    }
                },
            };

            for (keystrokes, action) in &section.bindings {
                let translated = translate_keystrokes(keystrokes, modifier);
                let result = build_action(action, cx).and_then(|(action, input)| {
                    KeyBinding::load(
                        &translated,
                        action,
                        predicate.clone(),
                        section.use_key_equivalents,
                        input,
                        cx.keyboard_mapper().as_ref(),
                    )
                    .map_err(|error| BindingErrorKind::InvalidKeystroke(error.keystroke))
                });
                match result {
                    Ok(binding) => loaded.bindings.push(binding),
                    Err(kind) => loaded.errors.push(BindingError {
                        context: section.context.clone(),
                        keystrokes: keystrokes.clone(),
                        kind,
                    }),
                }
            }
        }
        loaded
    }
}

/// Builds the action for one binding, plus its JSON input for display.
fn build_action(
    action: &KeymapAction,
    cx: &App,
) -> Result<(Box<dyn Action>, Option<SharedString>), BindingErrorKind> {
    let (name, data) = match action {
        KeymapAction::Unbind => return Ok((NoAction.boxed_clone(), None)),
        KeymapAction::Name(name) => (name, None),
        KeymapAction::WithArguments(name, data) => (name, Some(data.clone())),
    };
    let input = data
        .as_ref()
        .map(|data| SharedString::from(data.to_string()));
    match cx.build_action(name, data) {
        Ok(action) => Ok((action, input)),
        Err(gpui::ActionBuildError::NotFound { name }) => {
            Err(BindingErrorKind::UnknownAction(name))
        }
        Err(gpui::ActionBuildError::BuildError { name, error }) => {
            Err(BindingErrorKind::InvalidArguments {
                name,
                message: error.to_string(),
            })
        }
    }
}

/// Parses `source` and binds every valid binding. Skipped bindings are logged and
/// returned so a settings UI can show them.
pub fn bind_keymap(source: &str, cx: &mut App) -> Result<Vec<BindingError>, KeymapError> {
    let file = KeymapFile::parse(source)?;
    let LoadedKeymap { bindings, errors } = file.load(cx);
    for error in &errors {
        tracing::warn!(%error, "skipped key binding");
    }
    tracing::debug!(count = bindings.len(), "binding keys");
    cx.bind_keys(bindings);
    Ok(errors)
}

/// Removes `//` and `/* */` comments and trailing commas, so JSONC keymaps (as Zed
/// writes them) parse with `serde_json`. String contents are left alone.
pub fn strip_jsonc(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(escaped) = chars.next() {
                        out.push(escaped);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = '\0';
                for next in chars.by_ref() {
                    if previous == '*' && next == '/' {
                        break;
                    }
                    if next == '\n' {
                        out.push('\n');
                    }
                    previous = next;
                }
            }
            _ => out.push(c),
        }
    }
    remove_trailing_commas(&out)
}

/// Drops commas that are followed only by whitespace and a closing bracket.
fn remove_trailing_commas(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(&escaped) = chars.get(i + 1) {
                    out.push(escaped);
                    i += 1;
                }
            } else if c == '"' {
                in_string = false;
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, Some(']') | Some('}')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use gpui::{
        Context, FocusHandle, InteractiveElement, IntoElement, ParentElement, Render,
        TestAppContext, Window, actions, div,
    };

    use super::*;

    actions!(test_keymap, [Archive, NextUnread, TogglePane]);

    #[test]
    fn strips_comments_and_trailing_commas() {
        let source = r#"
            // A comment
            [
              {
                "context": "A", /* inline */
                "bindings": { "a": "x::Y", "b": "http://not-a-comment", },
              },
            ]
        "#;
        let file = KeymapFile::parse(source).unwrap();
        assert_eq!(file.0.len(), 1);
        assert_eq!(file.0[0].context.as_deref(), Some("A"));
        assert_eq!(
            file.0[0].bindings.get("b"),
            Some(&KeymapAction::Name("http://not-a-comment".into()))
        );
    }

    #[test]
    fn parses_all_action_forms() {
        let file = KeymapFile::parse(
            r#"[{ "bindings": { "a": "x::A", "b": ["x::B", { "n": 1 }], "c": null } }]"#,
        )
        .unwrap();
        let bindings = &file.0[0].bindings;
        assert_eq!(bindings["a"].name(), Some("x::A"));
        assert_eq!(
            bindings["b"],
            KeymapAction::WithArguments("x::B".into(), serde_json::json!({ "n": 1 }))
        );
        assert_eq!(bindings["c"], KeymapAction::Unbind);
        assert!(file.0[0].context.is_none());
    }

    #[test]
    fn default_keymap_parses() {
        let file = KeymapFile::parse(DEFAULT_MACOS_KEYMAP).unwrap();
        assert!(!file.0.is_empty());
    }

    #[gpui::test]
    fn reports_bad_bindings_and_keeps_the_rest(cx: &mut TestAppContext) {
        let file = KeymapFile::parse(
            r#"[
              { "context": "ThreadList", "bindings": {
                  "a": "test_keymap::Archive",
                  "q": "test_keymap::DoesNotExist",
                  "foo-bar": "test_keymap::NextUnread"
              } },
              { "context": "((", "bindings": { "b": "test_keymap::Archive" } }
            ]"#,
        )
        .unwrap();
        cx.update(|cx| {
            let loaded = file.load(cx);
            assert_eq!(loaded.bindings.len(), 1);
            assert_eq!(loaded.errors.len(), 3);
            assert!(loaded.errors.iter().any(|e| matches!(
                &e.kind,
                BindingErrorKind::UnknownAction(name) if name == "test_keymap::DoesNotExist"
            )));
            assert!(
                loaded
                    .errors
                    .iter()
                    .any(|e| matches!(e.kind, BindingErrorKind::InvalidContext { .. }))
            );
        });
    }

    /// Two nested key contexts, recording which actions reach which view.
    struct Harness {
        outer_focus: FocusHandle,
        inner_focus: FocusHandle,
        log: Vec<&'static str>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
            div()
                .key_context("Workspace")
                .track_focus(&self.outer_focus)
                .on_action(cx.listener(|this, _: &TogglePane, _, _| this.log.push("toggle")))
                .child(
                    div()
                        .key_context("ThreadList")
                        .track_focus(&self.inner_focus)
                        .on_action(cx.listener(|this, _: &Archive, _, _| this.log.push("archive")))
                        .on_action(cx.listener(|this, _: &NextUnread, _, _| this.log.push("next"))),
                )
        }
    }

    #[gpui::test]
    fn dispatches_by_context(cx: &mut TestAppContext) {
        let keymap = r#"[
          { "context": "Workspace", "bindings": { "cmd-b": "test_keymap::TogglePane" } },
          { "context": "ThreadList", "bindings": {
              "a": "test_keymap::Archive",
              "n": "test_keymap::NextUnread"
          } },
          { "context": "ThreadList", "bindings": { "n": null } }
        ]"#;
        cx.update(|cx| {
            let errors = bind_keymap(keymap, cx).unwrap();
            assert!(errors.is_empty(), "{errors:?}");
        });
        let (view, cx) = cx.add_window_view(|_, cx| Harness {
            outer_focus: cx.focus_handle(),
            inner_focus: cx.focus_handle(),
            log: Vec::new(),
        });

        // Focus inside the list: list bindings and workspace bindings both apply.
        cx.update(|window, cx| {
            let focus = view.read(cx).inner_focus.clone();
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("a cmd-b n");
        assert_eq!(
            view.read_with(cx, |v, _| v.log.clone()),
            ["archive", "toggle"]
        );

        // Focus on the workspace only: list bindings don't fire.
        cx.update(|window, cx| {
            let focus = view.read(cx).outer_focus.clone();
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("a cmd-b");
        assert_eq!(
            view.read_with(cx, |v, _| v.log.clone()),
            ["archive", "toggle", "toggle"]
        );
    }

    #[test]
    fn cmd_becomes_the_primary_modifier() {
        use PrimaryModifier::{Ctrl, Super};
        assert_eq!(translate_keystrokes("cmd-shift-f", Super), "cmd-shift-f");
        assert_eq!(translate_keystrokes("cmd-shift-f", Ctrl), "ctrl-shift-f");
        assert_eq!(translate_keystrokes("a cmd-b n", Ctrl), "a ctrl-b n");
        // Keys that are themselves `-` or punctuation stay.
        assert_eq!(translate_keystrokes("cmd--", Ctrl), "ctrl--");
        assert_eq!(translate_keystrokes("cmd-\\", Ctrl), "ctrl-\\");
        // Plain ctrl bindings stay; cmd and ctrl together stay two modifiers.
        assert_eq!(translate_keystrokes("ctrl-tab", Ctrl), "ctrl-tab");
        assert_eq!(
            translate_keystrokes("cmd-ctrl-left", Ctrl),
            "ctrl-super-left"
        );
    }

    #[test]
    fn shortcuts_read_as_symbols_or_text() {
        assert_eq!(format_shortcut("cmd-shift-f", true), "⇧⌘F");
        assert_eq!(format_shortcut("ctrl-shift-f", false), "Ctrl+Shift+F");
        assert_eq!(format_shortcut("ctrl-super-left", false), "Ctrl+Super+Left");
        assert_eq!(format_shortcut("cmd-enter", true), "⌘↩");
        assert_eq!(format_shortcut("cmd-,", false), "Super+,");
        assert_eq!(format_shortcut("f8", false), "F8");
    }

    #[test]
    fn the_default_keymap_has_no_clashes_with_either_modifier() {
        let file = KeymapFile::parse(DEFAULT_MACOS_KEYMAP).unwrap();
        for modifier in [PrimaryModifier::Super, PrimaryModifier::Ctrl] {
            let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
            for section in &file.0 {
                let context = section.context.clone().unwrap_or_default();
                for (keys, action) in &section.bindings {
                    let Some(name) = action.name() else { continue };
                    let translated = translate_keystrokes(keys, modifier);
                    if let Some(other) =
                        seen.insert((context.clone(), translated.clone()), name.to_owned())
                        && other != name
                    {
                        panic!("{modifier:?}: {translated} in {context:?} is {other} and {name}");
                    }
                }
            }
        }
    }

    #[gpui::test]
    fn ctrl_as_the_primary_modifier_binds_ctrl(cx: &mut TestAppContext) {
        cx.update(|cx| {
            set_primary_modifier(PrimaryModifier::Ctrl, cx);
            let errors = bind_keymap(
                r#"[{ "context": "Workspace", "bindings": { "cmd-b": "test_keymap::TogglePane" } }]"#,
                cx,
            )
            .unwrap();
            assert!(errors.is_empty(), "{errors:?}");
            assert!(shortcut_label("cmd-b", cx).ends_with('B'));
        });
        let (view, cx) = cx.add_window_view(|_, cx| Harness {
            outer_focus: cx.focus_handle(),
            inner_focus: cx.focus_handle(),
            log: Vec::new(),
        });
        cx.update(|window, cx| {
            let focus = view.read(cx).outer_focus.clone();
            window.focus(&focus, cx);
        });
        cx.simulate_keystrokes("cmd-b ctrl-b");
        assert_eq!(view.read_with(cx, |v, _| v.log.clone()), ["toggle"]);
    }
}

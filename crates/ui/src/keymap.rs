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

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, NoAction, SharedString};
use serde::Deserialize;

/// Maze's default macOS keymap (Thunderbird bindings plus Zed's, CONCEPT §12).
pub const DEFAULT_MACOS_KEYMAP: &str = include_str!("../../../assets/keymaps/default-macos.json");

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

    /// Builds GPUI key bindings. Actions must be registered (any `actions!` type
    /// linked into the binary is) before this runs.
    pub fn load(&self, cx: &App) -> LoadedKeymap {
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
                let result = build_action(action, cx).and_then(|(action, input)| {
                    KeyBinding::load(
                        keystrokes,
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
}

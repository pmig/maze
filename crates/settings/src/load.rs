//! Parsing, merging and validating settings.
//!
//! The user file is merged over the embedded defaults as JSON values (objects merge
//! key by key, everything else replaces), then deserialized into [`Settings`]:
//!
//! - unknown keys become [`DiagnosticKind::UnknownKey`] warnings and are ignored;
//! - an invalid value becomes a [`DiagnosticKind::InvalidValue`] error with its JSON
//!   path, and only that key falls back to its default;
//! - a syntax error in the user file ignores the whole file (defaults apply) and
//!   reports the line and column.
//!
//! Loading never fails: there is always a usable [`Settings`].

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::LazyLock;

use jsonc_parser::ParseOptions;
use serde_json::{Map, Value};

use crate::Settings;

/// The default settings as shipped (`assets/settings/default.jsonc`).
pub const DEFAULT_SETTINGS: &str = include_str!("../../../assets/settings/default.jsonc");

/// Upper bound on repair rounds (one per invalid value) before giving up on the user
/// file entirely.
const MAX_REPAIRS: usize = 64;

/// Keys that are valid in the file without being settings.
const META_KEYS: &[&str] = &["$schema"];

static DEFAULT_VALUE: LazyLock<Value> = LazyLock::new(|| {
    // A test guarantees that the embedded defaults parse and match `Settings::default()`.
    parse_jsonc(DEFAULT_SETTINGS).unwrap_or_else(|_| Value::Object(Map::new()))
});

/// Settings together with everything that was wrong with the user file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadedSettings {
    /// The usable settings.
    pub settings: Settings,
    /// Problems found while loading. Empty if the file was fine (or absent).
    pub diagnostics: Vec<Diagnostic>,
}

impl LoadedSettings {
    /// Whether any diagnostic is an error (as opposed to a warning).
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(Diagnostic::is_error)
    }
}

/// One problem with the user's settings file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// What went wrong.
    pub kind: DiagnosticKind,
    /// The JSON path of the offending key, e.g. `agent_panel.width` or
    /// `folder_pane.modes[1]`. Empty for the whole file.
    pub path: String,
    /// A readable description.
    pub message: String,
}

/// The kind of a [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiagnosticKind {
    /// The file couldn't be read. Defaults apply.
    Io,
    /// The file isn't valid JSONC. Defaults apply; the message has line and column.
    Syntax,
    /// A value has the wrong type or is out of range. That key uses its default.
    InvalidValue,
    /// A key Maze doesn't know. It is ignored.
    UnknownKey,
}

impl Diagnostic {
    /// Unknown keys are warnings; everything else is an error.
    pub fn is_error(&self) -> bool {
        !matches!(self.kind, DiagnosticKind::UnknownKey)
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

/// The embedded defaults as typed settings.
pub fn default_settings() -> Settings {
    parse_settings(None).settings
}

/// Loads the user's settings file and merges it over the defaults. A missing file is
/// not an error.
pub fn load_settings(path: &Path) -> LoadedSettings {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_settings(Some(&text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => parse_settings(None),
        Err(err) => {
            let mut loaded = parse_settings(None);
            loaded.diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Io,
                path: String::new(),
                message: format!("cannot read {}: {err}", path.display()),
            });
            loaded
        }
    }
}

/// Parses user settings text (JSONC) and merges it over the defaults. `None` means
/// there is no user file.
pub fn parse_settings(user_text: Option<&str>) -> LoadedSettings {
    let mut diagnostics = Vec::new();
    let mut merged = DEFAULT_VALUE.clone();

    if let Some(text) = user_text {
        match parse_jsonc(text) {
            Ok(Value::Object(user)) => merge(&mut merged, Value::Object(user)),
            // An empty file parses as null: nothing to merge.
            Ok(Value::Null) => {}
            Ok(other) => diagnostics.push(Diagnostic {
                kind: DiagnosticKind::InvalidValue,
                path: String::new(),
                message: format!(
                    "settings must be a JSON object, found {}",
                    value_kind(&other)
                ),
            }),
            Err(err) => diagnostics.push(Diagnostic {
                kind: DiagnosticKind::Syntax,
                path: String::new(),
                message: err.to_string(),
            }),
        }
    }

    for _ in 0..MAX_REPAIRS {
        match deserialize(merged.clone()) {
            Ok((settings, ignored)) => {
                diagnostics.extend(ignored.into_iter().map(|path| Diagnostic {
                    message: "unknown setting, ignored".to_owned(),
                    kind: DiagnosticKind::UnknownKey,
                    path,
                }));
                // Report in a stable order. serde_json's map order depends on whether
                // some other crate in the build enables its `preserve_order` feature.
                diagnostics.sort_by(|a, b| a.path.cmp(&b.path));
                return LoadedSettings {
                    settings,
                    diagnostics,
                };
            }
            Err(err) => {
                let segments = error_segments(err.path());
                diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::InvalidValue,
                    path: format_path(&segments),
                    message: format!("{}; using the default", err.inner()),
                });
                if !repair(&mut merged, &DEFAULT_VALUE, segments) {
                    break;
                }
            }
        }
    }

    // Unreachable unless the defaults themselves are broken.
    diagnostics.push(Diagnostic {
        kind: DiagnosticKind::InvalidValue,
        path: String::new(),
        message: "settings could not be applied; using the defaults".to_owned(),
    });
    LoadedSettings {
        settings: Settings::default(),
        diagnostics,
    }
}

pub(crate) fn parse_jsonc(text: &str) -> Result<Value, jsonc_parser::errors::ParseError> {
    // Comments and trailing commas, nothing looser: files stay readable by editors
    // that validate against the JSON Schema.
    let options = ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        ..ParseOptions::default()
    };
    jsonc_parser::parse_to_serde_value(text, &options)
}

type DeserializeError = serde_path_to_error::Error<serde_json::Error>;

/// Deserializes, collecting the paths of ignored (unknown) keys.
fn deserialize(value: Value) -> Result<(Settings, Vec<String>), DeserializeError> {
    let mut ignored = Vec::new();
    let mut on_ignored = |path: serde_ignored::Path<'_>| {
        let mut segments = Vec::new();
        ignored_segments(&path, &mut segments);
        let is_meta =
            matches!(segments.as_slice(), [Segment::Key(key)] if META_KEYS.contains(&key.as_str()));
        if !is_meta {
            ignored.push(format_path(&segments));
        }
    };
    let settings =
        serde_path_to_error::deserialize(serde_ignored::Deserializer::new(value, &mut on_ignored))?;
    Ok((settings, ignored))
}

/// Overlays `overlay` onto `base`: objects merge recursively, anything else replaces.
fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(slot) if slot.is_object() && value.is_object() => merge(slot, value),
                    _ => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Key(String),
    Index(usize),
}

fn error_segments(path: &serde_path_to_error::Path) -> Vec<Segment> {
    let mut segments = Vec::new();
    for segment in path.iter() {
        match segment {
            serde_path_to_error::Segment::Map { key } => segments.push(Segment::Key(key.clone())),
            serde_path_to_error::Segment::Seq { index } => segments.push(Segment::Index(*index)),
            // Settings use no data-carrying enums; stop at anything we can't address.
            serde_path_to_error::Segment::Enum { .. } | serde_path_to_error::Segment::Unknown => {
                break;
            }
        }
    }
    segments
}

fn ignored_segments(path: &serde_ignored::Path<'_>, out: &mut Vec<Segment>) {
    match path {
        serde_ignored::Path::Root => {}
        serde_ignored::Path::Seq { parent, index } => {
            ignored_segments(parent, out);
            out.push(Segment::Index(*index));
        }
        serde_ignored::Path::Map { parent, key } => {
            ignored_segments(parent, out);
            out.push(Segment::Key(key.clone()));
        }
        serde_ignored::Path::Some { parent }
        | serde_ignored::Path::NewtypeStruct { parent }
        | serde_ignored::Path::NewtypeVariant { parent } => ignored_segments(parent, out),
    }
}

/// `agent_panel.width`, `folder_pane.modes[1]`, `accounts["my.work"].offline`.
fn format_path(segments: &[Segment]) -> String {
    let mut out = String::new();
    for segment in segments {
        match segment {
            Segment::Key(key) if is_identifier(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Segment::Key(key) => {
                out.push('[');
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(']');
            }
            Segment::Index(index) => {
                out.push('[');
                out.push_str(&index.to_string());
                out.push(']');
            }
        }
    }
    out
}

fn is_identifier(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '$')
}

fn get<'a>(value: &'a Value, segments: &[Segment]) -> Option<&'a Value> {
    segments
        .iter()
        .try_fold(value, |value, segment| match segment {
            Segment::Key(key) => value.as_object()?.get(key),
            Segment::Index(index) => value.as_array()?.get(*index),
        })
}

fn get_mut<'a>(value: &'a mut Value, segments: &[Segment]) -> Option<&'a mut Value> {
    segments
        .iter()
        .try_fold(value, |value, segment| match segment {
            Segment::Key(key) => value.as_object_mut()?.get_mut(key),
            Segment::Index(index) => value.as_array_mut()?.get_mut(*index),
        })
}

/// Replaces the invalid value at `segments` with its default, or removes it if it has
/// none (the struct's `Default` then applies). Arrays are one value: an invalid element
/// resets the whole array. Walks up to the parent when neither is possible. Returns
/// whether anything changed.
fn repair(merged: &mut Value, defaults: &Value, mut segments: Vec<Segment>) -> bool {
    if let Some(first_index) = segments
        .iter()
        .position(|segment| matches!(segment, Segment::Index(_)))
    {
        segments.truncate(first_index);
    }
    loop {
        if let Some(default) = get(defaults, &segments) {
            if let Some(slot) = get_mut(merged, &segments)
                && slot != default
            {
                *slot = default.clone();
                return true;
            }
        } else if let Some((Segment::Key(key), parent)) = segments.split_last()
            && let Some(Value::Object(parent)) = get_mut(merged, parent)
            && parent.remove(key).is_some()
        {
            return true;
        }
        if segments.pop().is_none() {
            return false;
        }
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{
        AccountOverrides, AgentAccess, FolderPaneMode, Layout, OfflinePolicy, RemoteContentPolicy,
        ThemeMode, UpdateChannel,
    };

    #[test]
    fn embedded_defaults_parse_cleanly() {
        let value = parse_jsonc(DEFAULT_SETTINGS).expect("default.jsonc must parse");
        let loaded = parse_settings(None);
        assert_eq!(loaded.diagnostics, vec![]);
        // Every key in default.jsonc is known, so nothing was ignored.
        let (_, ignored) = deserialize(value).unwrap();
        assert_eq!(ignored, Vec::<String>::new());
    }

    #[test]
    fn embedded_defaults_match_rust_defaults() {
        // The Rust `Default` impls are the per-key fallback; they must say the same as
        // the file people read.
        let file = parse_jsonc(DEFAULT_SETTINGS).unwrap();
        let rust = serde_json::to_value(Settings::default()).unwrap();
        assert_eq!(file, rust);
        assert_eq!(default_settings(), Settings::default());
    }

    #[test]
    fn user_values_override_defaults() {
        let loaded = parse_settings(Some(
            r#"
            // my settings
            {
              "$schema": "file:///tmp/settings.json",
              "theme": { "mode": "dark" },          // keeps light/dark names
              "layout": "classic",
              "folder_pane": { "modes": ["all", "tags",], },
              "accounts": { "work": { "offline": "all", "remote_content": "never" } },
              "agent": { "accounts": { "work": "any", "personal": "local_only" } },
              "update_channel": "preview",
            }
            "#,
        ));
        assert_eq!(loaded.diagnostics, vec![]);
        let s = loaded.settings;
        assert_eq!(s.theme.mode, ThemeMode::Dark);
        assert_eq!(s.theme.light, "One Light");
        assert_eq!(s.layout, Layout::Classic);
        assert_eq!(
            s.folder_pane.modes,
            vec![FolderPaneMode::All, FolderPaneMode::Tags]
        );
        assert_eq!(
            s.accounts.get("work"),
            Some(&AccountOverrides {
                offline: Some(OfflinePolicy::All),
                offline_recent_days: None,
                remote_content: Some(RemoteContentPolicy::Never),
            })
        );
        assert_eq!(s.agent.access_for("personal"), AgentAccess::LocalOnly);
        assert_eq!(s.agent.default.as_deref(), Some("claude"));
        assert_eq!(s.update_channel, Some(UpdateChannel::Preview));
    }

    #[test]
    fn unknown_keys_are_warnings() {
        let loaded = parse_settings(Some(
            r#"{ "lay_out": "wide", "reader": { "zoom": 2 }, "accounts": { "a.b": { "colour": 1 } } }"#,
        ));
        let paths: Vec<_> = loaded
            .diagnostics
            .iter()
            .map(|d| (d.kind, d.path.as_str()))
            .collect();
        assert_eq!(
            paths,
            vec![
                (DiagnosticKind::UnknownKey, "accounts[\"a.b\"].colour"),
                (DiagnosticKind::UnknownKey, "lay_out"),
                (DiagnosticKind::UnknownKey, "reader.zoom"),
            ]
        );
        assert!(!loaded.has_errors());
        assert_eq!(loaded.settings.layout, Layout::FourColumn);
    }

    #[test]
    fn invalid_values_fall_back_per_key() {
        let loaded = parse_settings(Some(
            r#"{
              "layout": "diagonal",
              "agent_panel": { "width": "wide", "visible": false },
              "compose": { "undo_send_seconds": 99, "format": "plain" },
              "folder_pane": { "modes": ["all", 7] },
              "accounts": { "work": { "offline": "sometimes", "remote_content": "never" } },
              "log_level": "debug"
            }"#,
        ));
        let errors: Vec<_> = loaded
            .diagnostics
            .iter()
            .filter(|d| d.kind == DiagnosticKind::InvalidValue)
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(
            errors,
            vec![
                "accounts.work.offline",
                "agent_panel.width",
                "compose.undo_send_seconds",
                "folder_pane.modes[1]",
                "layout",
            ]
        );
        let s = &loaded.settings;
        // Invalid keys use their defaults ...
        assert_eq!(s.layout, Layout::FourColumn);
        assert_eq!(s.agent_panel.width, 380);
        assert_eq!(s.compose.undo_send_seconds.get(), 10);
        assert_eq!(
            s.folder_pane.modes,
            vec![FolderPaneMode::Favorite, FolderPaneMode::Unified]
        );
        // ... while valid siblings are kept.
        assert!(!s.agent_panel.visible);
        assert_eq!(s.compose.format, crate::ComposeFormat::Plain);
        assert_eq!(s.log_level, crate::LogLevel::Debug);
        let work = s.account("work");
        assert_eq!(work.offline, OfflinePolicy::Recent);
        assert_eq!(work.remote_content, RemoteContentPolicy::Never);

        let layout = loaded
            .diagnostics
            .iter()
            .find(|d| d.path == "layout")
            .unwrap();
        let message = layout.to_string();
        assert!(
            message.starts_with("layout: unknown variant `diagonal`"),
            "{message}"
        );
        assert!(message.ends_with("using the default"), "{message}");
    }

    #[test]
    fn syntax_errors_keep_defaults() {
        let loaded = parse_settings(Some("{ \"layout\": \"wide\",\n \"theme\": }"));
        assert_eq!(loaded.diagnostics.len(), 1);
        let diagnostic = &loaded.diagnostics[0];
        assert_eq!(diagnostic.kind, DiagnosticKind::Syntax);
        assert!(
            diagnostic.message.contains("line 2"),
            "{}",
            diagnostic.message
        );
        assert_eq!(loaded.settings, Settings::default());
    }

    #[test]
    fn empty_and_non_object_files() {
        assert_eq!(parse_settings(Some("")).diagnostics, vec![]);
        assert_eq!(
            parse_settings(Some("  // nothing yet\n")).diagnostics,
            vec![]
        );
        let loaded = parse_settings(Some("[1, 2]"));
        assert_eq!(loaded.diagnostics[0].kind, DiagnosticKind::InvalidValue);
        assert_eq!(loaded.settings, Settings::default());
    }

    #[test]
    fn null_resets_optional_values() {
        let loaded = parse_settings(Some(r#"{ "agent": { "default": null }, "layout": null }"#));
        assert_eq!(loaded.settings.agent.default, None);
        // `layout` can't be null: error, default kept.
        assert_eq!(loaded.diagnostics.len(), 1);
        assert_eq!(loaded.diagnostics[0].path, "layout");
    }

    #[test]
    fn load_settings_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(load_settings(&path), LoadedSettings::default());

        std::fs::write(&path, r#"{ "auto_update": false }"#).unwrap();
        let loaded = load_settings(&path);
        assert!(!loaded.settings.auto_update);
        assert!(loaded.diagnostics.is_empty());

        // A directory where the file should be is an IO error, not a panic.
        let loaded = load_settings(dir.path());
        assert_eq!(loaded.diagnostics[0].kind, DiagnosticKind::Io);
        assert_eq!(loaded.settings, Settings::default());
    }

    #[test]
    fn merge_semantics() {
        let mut base = serde_json::json!({ "a": { "b": 1, "c": [1, 2] }, "d": 1 });
        merge(
            &mut base,
            serde_json::json!({ "a": { "c": [3] }, "d": { "e": 1 } }),
        );
        assert_eq!(
            base,
            serde_json::json!({ "a": { "b": 1, "c": [3] }, "d": { "e": 1 } })
        );
    }
}

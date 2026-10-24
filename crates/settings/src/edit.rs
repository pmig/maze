//! Writing the user's settings file.
//!
//! Maze changes single keys of `settings.json` itself (the favorite folders). The file
//! is people's, so an edit keeps everything else as it was: comments, formatting,
//! key order and keys Maze doesn't know. The JSONC is edited as a concrete syntax tree
//! (`jsonc-parser`'s CST), never re-serialized from [`Settings`](crate::Settings).
//!
//! A file that isn't valid JSONC is left alone ([`EditError::Syntax`]): rewriting it
//! would throw away what the user is in the middle of typing.

use std::io;
use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstObject, CstRootNode};
use serde_json::Value;

use crate::load::jsonc_options;

/// Why the settings file couldn't be changed.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// Reading or writing the file failed.
    #[error("cannot write {}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// The file isn't valid JSONC; it was not changed.
    #[error("{} has a syntax error, fix it first: {message}", path.display())]
    Syntax {
        /// The file.
        path: PathBuf,
        /// The parser's message with line and column.
        message: String,
    },
    /// A key on the way to the value holds something other than an object.
    #[error("`{key}` in {} is not an object", path.display())]
    NotAnObject {
        /// The file.
        path: PathBuf,
        /// The dotted key, e.g. `folder_pane`.
        key: String,
    },
}

/// Sets the value at `keys` (object keys from the top, e.g. `["folder_pane",
/// "favorites"]`) in the settings file at `file`, creating the file and any missing
/// objects on the way. Comments, formatting and every other key are kept. The file is
/// replaced atomically (temporary file and rename), so the settings watcher never
/// reads half of it; a symlinked file is written where the link points.
///
/// `keys` must not be empty.
pub fn write_setting(file: &Path, keys: &[&str], value: &Value) -> Result<(), EditError> {
    let io_error = |source| EditError::Io {
        path: file.to_path_buf(),
        source,
    };
    let Some((last, parents)) = keys.split_last() else {
        return Err(io_error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no settings key given",
        )));
    };
    // Write through a symlink (dotfile repositories), not over it.
    let target = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let text = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(io_error(err)),
    };
    let root = CstRootNode::parse(&text, &jsonc_options()).map_err(|err| EditError::Syntax {
        path: file.to_path_buf(),
        message: err.to_string(),
    })?;
    let not_an_object = |depth: usize| EditError::NotAnObject {
        path: file.to_path_buf(),
        key: keys[..depth].join("."),
    };
    let mut object: CstObject = root
        .object_value_or_create()
        .ok_or_else(|| not_an_object(0))?;
    for (depth, key) in parents.iter().enumerate() {
        object = object
            .object_value_or_create(key)
            .ok_or_else(|| not_an_object(depth + 1))?;
    }
    let input = cst_value(value);
    match object.get(last) {
        Some(prop) => prop.set_value(input),
        None => {
            object.append(last, input);
        }
    }
    let mut out = root.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    replace_file(&target, &out).map_err(io_error)
}

/// What a new `settings.json` holds: a pointer to the defaults and an empty object.
pub const USER_SETTINGS_TEMPLATE: &str = "\
// Your Maze settings. Add only the keys you want to change; they are merged over the
// defaults (\"Maze: Open Default Settings\" in the command palette shows them all, with
// comments). Comments and trailing commas are allowed.
{
}
";

/// Replaces the whole settings file with `text` (the settings editor's Save): written
/// atomically (temporary file and rename, so the settings watcher never reads half of
/// it), through a symlink rather than over it, with a trailing newline. The text is
/// written as it is, even if it isn't valid JSONC: it is what the user typed.
pub fn write_settings_file(file: &Path, text: &str) -> Result<(), EditError> {
    let target = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let mut out = text.replace("\r\n", "\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    replace_file(&target, &out).map_err(|source| EditError::Io {
        path: file.to_path_buf(),
        source,
    })
}

/// Reads the settings file, creating it from [`USER_SETTINGS_TEMPLATE`] first if it
/// doesn't exist yet.
pub fn read_or_create_settings_file(file: &Path) -> Result<String, EditError> {
    let io_error = |source| EditError::Io {
        path: file.to_path_buf(),
        source,
    };
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            write_settings_file(file, USER_SETTINGS_TEMPLATE)?;
            std::fs::read_to_string(file).map_err(io_error)
        }
        Err(err) => Err(io_error(err)),
    }
}

/// A JSON value as CST input.
fn cst_value(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(cst_value).collect()),
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), cst_value(value)))
                .collect(),
        ),
    }
}

/// Writes `text` to a temporary file next to `path` and renames it over `path`.
fn replace_file(path: &Path, text: &str) -> io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the settings path has no file name",
        ));
    };
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, text)?;
    if let Err(err) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;
    use crate::{FavoriteFolder, load_settings};

    fn favorites() -> Value {
        json!([{ "account": "work", "folder": "Customers/Acme" }])
    }

    #[test]
    fn creates_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/settings.json");
        write_setting(&path, &["folder_pane", "favorites"], &favorites()).unwrap();
        let loaded = load_settings(&path);
        assert_eq!(loaded.diagnostics, vec![]);
        assert_eq!(
            loaded.settings.folder_pane.favorites,
            vec![FavoriteFolder {
                account: "work".to_owned(),
                folder: "Customers/Acme".to_owned(),
            }]
        );
    }

    #[test]
    fn keeps_comments_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let before = r#"{
  // Dark, always.
  "theme": { "mode": "dark" },
  "folder_pane": {
    "modes": ["unified"], // no favorites mode
  },
}
"#;
        std::fs::write(&path, before).unwrap();
        write_setting(&path, &["folder_pane", "favorites"], &favorites()).unwrap();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("// Dark, always."), "{after}");
        assert!(after.contains("// no favorites mode"), "{after}");
        assert!(after.contains(r#""theme": { "mode": "dark" }"#), "{after}");
        let loaded = load_settings(&path);
        assert_eq!(loaded.diagnostics, vec![]);
        assert_eq!(loaded.settings.theme.mode, crate::ThemeMode::Dark);
        assert_eq!(
            loaded.settings.folder_pane.modes,
            vec![crate::FolderPaneMode::Unified]
        );
        assert_eq!(loaded.settings.folder_pane.favorites.len(), 1);

        // Replacing the value keeps the rest again; an empty list is written as such.
        write_setting(&path, &["folder_pane", "favorites"], &json!([])).unwrap();
        let again = std::fs::read_to_string(&path).unwrap();
        assert!(again.contains("// Dark, always."), "{again}");
        assert_eq!(load_settings(&path).settings.folder_pane.favorites, vec![]);
    }

    #[test]
    fn leaves_broken_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "theme": "#).unwrap();
        let err = write_setting(&path, &["folder_pane", "favorites"], &favorites());
        assert!(matches!(err, Err(EditError::Syntax { .. })), "{err:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{ "theme": "#);

        std::fs::write(&path, r#"{ "folder_pane": [] }"#).unwrap();
        let err = write_setting(&path, &["folder_pane", "favorites"], &favorites());
        assert!(
            matches!(&err, Err(EditError::NotAnObject { key, .. }) if key == "folder_pane"),
            "{err:?}"
        );
    }

    #[test]
    fn the_whole_file_is_written_with_a_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/settings.json");
        // Created from the template, which loads cleanly.
        let text = read_or_create_settings_file(&path).unwrap();
        assert_eq!(text, USER_SETTINGS_TEMPLATE);
        assert!(text.contains("Open Default Settings"));
        assert_eq!(load_settings(&path).diagnostics, vec![]);
        // An existing file is read as it is.
        std::fs::write(&path, "{ \"layout\": \"wide\" }").unwrap();
        assert_eq!(
            read_or_create_settings_file(&path).unwrap(),
            "{ \"layout\": \"wide\" }"
        );

        write_settings_file(&path, "{ \"layout\": \"classic\" }").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ \"layout\": \"classic\" }\n"
        );
        // Broken text is written too (it's what the user typed); no temp file stays.
        write_settings_file(&path, "{ \"layout\": \n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ \"layout\": \n");
        let files: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "{}\n").unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_setting(&link, &["folder_pane", "favorites"], &favorites()).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(load_settings(&real).settings.folder_pane.favorites.len(), 1);
    }
}

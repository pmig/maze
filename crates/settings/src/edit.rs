//! Writing the user's settings file.
//!
//! Maze changes single keys of `settings.json` itself (the favorite folders, and an
//! account's settings when it is renamed). The file is people's, so an edit keeps
//! everything else as it was: comments, formatting, key order and keys Maze doesn't
//! know. The JSONC is edited as a concrete syntax tree (`jsonc-parser`'s CST), never
//! re-serialized from [`Settings`](crate::Settings).
//!
//! A file that isn't valid JSONC is left alone ([`EditError::Syntax`]): rewriting it
//! would throw away what the user is in the middle of typing.

use std::io;
use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstNode, CstObject, CstRootNode, ObjectPropName};
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
    /// Renaming an account: the file has settings under both the old and the new name
    /// in the same place ([`rename_account`]); it was not changed.
    #[error(
        "{} has settings for both \u{201c}{old}\u{201d} and \u{201c}{new}\u{201d} in `{key}`: \
         merge or remove one of them first",
        path.display()
    )]
    NameTaken {
        /// The file.
        path: PathBuf,
        /// The account's current label.
        old: String,
        /// The label it was to get.
        new: String,
        /// Where both are, e.g. `agent.accounts`.
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

/// Moves what the settings file keeps for the account labelled `old` to the label
/// `new`, for a renamed account: `accounts.<old>` and `agent.accounts.<old>` are
/// renamed in place (their values, comments and formatting stay as they are), and the
/// account's favorite folders (`folder_pane.favorites`) get the new label. Returns
/// whether the file changed (`false` without a file or with nothing to move).
///
/// Settings that already exist under `new` stay and apply to the renamed account, as
/// for any account with that label, except where both labels have an entry in the same
/// object: that can't be merged, so the file is left alone with
/// [`EditError::NameTaken`]. A favorite listed under both labels is kept once. A file
/// that isn't valid JSONC is left alone ([`EditError::Syntax`]).
pub fn rename_account(file: &Path, old: &str, new: &str) -> Result<bool, EditError> {
    move_account(file, old, new, true)
}

/// Whether [`rename_account`] would succeed now, without writing anything: the errors
/// it would return, else `Ok`.
pub fn check_account_rename(file: &Path, old: &str, new: &str) -> Result<(), EditError> {
    move_account(file, old, new, false).map(|_| ())
}

fn move_account(file: &Path, old: &str, new: &str, write: bool) -> Result<bool, EditError> {
    if old == new {
        return Ok(false);
    }
    let target = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let text = match std::fs::read_to_string(&target) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(EditError::Io {
                path: file.to_path_buf(),
                source,
            });
        }
    };
    let root = CstRootNode::parse(&text, &jsonc_options()).map_err(|err| EditError::Syntax {
        path: file.to_path_buf(),
        message: err.to_string(),
    })?;
    let Some(object) = root.object_value() else {
        return Ok(false);
    };

    // Check both objects before touching either, so a refusal changes nothing.
    let maps = [
        ("accounts", object.object_value("accounts")),
        (
            "agent.accounts",
            object
                .object_value("agent")
                .and_then(|agent| agent.object_value("accounts")),
        ),
    ];
    let mut renames = Vec::new();
    for (key, map) in &maps {
        let Some(prop) = map.as_ref().and_then(|map| map.get(old)) else {
            continue;
        };
        if map.as_ref().and_then(|map| map.get(new)).is_some() {
            return Err(EditError::NameTaken {
                path: file.to_path_buf(),
                old: old.to_owned(),
                new: new.to_owned(),
                key: (*key).to_owned(),
            });
        }
        renames.push(prop);
    }
    let mut changed = !renames.is_empty();
    let quoted = serde_json::to_string(new).unwrap_or_else(|_| format!("\"{new}\""));
    for prop in renames {
        match prop.name() {
            Some(ObjectPropName::String(name)) => name.set_raw_value(quoted.clone()),
            Some(ObjectPropName::Word(name)) => name.set_raw_value(quoted.clone()),
            None => {}
        }
    }

    let favorites = object
        .object_value("folder_pane")
        .and_then(|pane| pane.array_value("favorites"));
    if let Some(favorites) = favorites {
        let entry = |node: &CstNode| {
            let favorite = node.as_object()?;
            let label = |key: &str| {
                favorite
                    .get(key)?
                    .value()?
                    .as_string_lit()?
                    .decoded_value()
                    .ok()
            };
            Some((
                favorite.get("account")?,
                label("account")?,
                label("folder")?,
            ))
        };
        let listed: Vec<(String, String)> = favorites
            .elements()
            .iter()
            .filter_map(|node| entry(node).map(|(_, account, folder)| (account, folder)))
            .collect();
        for node in favorites.elements() {
            let Some((account, label, folder)) = entry(&node) else {
                continue;
            };
            if label != old {
                continue;
            }
            if listed.contains(&(new.to_owned(), folder)) {
                node.remove();
            } else {
                account.set_value(CstInputValue::String(new.to_owned()));
            }
            changed = true;
        }
    }

    if !changed || !write {
        return Ok(changed);
    }
    let mut out = root.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    replace_file(&target, &out).map_err(|source| EditError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    Ok(true)
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
    fn renaming_an_account_moves_its_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let before = r#"{
  "accounts": {
    // My job.
    "Work": { "offline": "all", "copy_sent_to_inbox": true },
    "Home": { "sync_trash": false },
  },
  "agent": { "accounts": { "Work": "any", "Home": "off" } },
  "folder_pane": {
    "favorites": [
      { "account": "Work", "folder": "Customers/Acme" },
      { "account": "Home", "folder": "Inbox" },
      { "account": "Work", "folder": "Inbox" },
      { "account": "Job", "folder": "Inbox" },
    ],
  },
}
"#;
        std::fs::write(&path, before).unwrap();
        assert!(rename_account(&path, "Work", "Job").unwrap());
        let after = std::fs::read_to_string(&path).unwrap();
        // Renamed in place: the comment and the value's formatting stay.
        assert!(
            after.contains(
                "// My job.\n    \"Job\": { \"offline\": \"all\", \"copy_sent_to_inbox\": true },"
            ),
            "{after}"
        );
        let loaded = load_settings(&path);
        assert_eq!(loaded.diagnostics, vec![]);
        let settings = loaded.settings;
        assert!(settings.copy_sent_to_inbox("Job"));
        assert!(!settings.copy_sent_to_inbox("Work"));
        assert_eq!(settings.agent.access_for("Job"), crate::AgentAccess::Any);
        assert_eq!(settings.agent.access_for("Work"), crate::AgentAccess::Off);
        assert!(!settings.account("Home").sync_trash, "others stay");
        // "Job / Inbox" was a favorite already: listed once.
        let favorite = |account: &str, folder: &str| FavoriteFolder {
            account: account.to_owned(),
            folder: folder.to_owned(),
        };
        assert_eq!(
            settings.folder_pane.favorites,
            vec![
                favorite("Job", "Customers/Acme"),
                favorite("Home", "Inbox"),
                favorite("Job", "Inbox"),
            ]
        );

        // Nothing under the old name (any more): nothing to write.
        let unchanged = std::fs::read_to_string(&path).unwrap();
        assert!(!rename_account(&path, "Work", "Office").unwrap());
        assert!(!rename_account(&path, "Job", "Job").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), unchanged);
        // No file: nothing to move.
        assert!(!rename_account(&dir.path().join("none.json"), "Job", "Work").unwrap());
    }

    #[test]
    fn renaming_onto_existing_settings_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let before = r#"{
  "accounts": { "Work": { "offline": "all" } },
  "agent": { "accounts": { "Work": "off", "Job": "any" } },
}
"#;
        std::fs::write(&path, before).unwrap();
        let check = check_account_rename(&path, "Work", "Job");
        assert!(
            matches!(check, Err(EditError::NameTaken { .. })),
            "{check:?}"
        );
        let err = rename_account(&path, "Work", "Job").unwrap_err();
        assert!(
            matches!(&err, EditError::NameTaken { key, .. } if key == "agent.accounts"),
            "{err:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // Settings only under the new name are fine: they apply to the renamed account.
        assert!(check_account_rename(&path, "Home", "Job").is_ok());
        assert!(rename_account(&path, "Home", "Job").is_ok());
        std::fs::write(&path, r#"{ "accounts": "#).unwrap();
        assert!(matches!(
            rename_account(&path, "Work", "Job"),
            Err(EditError::Syntax { .. })
        ));
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

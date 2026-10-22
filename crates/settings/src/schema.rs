//! JSON Schema for `settings.json`, for editor autocompletion and validation.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::Settings;

/// The JSON Schema of [`Settings`].
///
/// Objects with fixed keys get `"additionalProperties": false`, so editors flag typos
/// the same way Maze warns about unknown keys. The root also allows `"$schema"`.
pub fn settings_json_schema() -> Value {
    let mut schema = schemars::schema_for!(Settings).to_value();
    close_objects(&mut schema);
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.insert(
            "$schema".to_owned(),
            serde_json::json!({
                "type": "string",
                "description": "The JSON Schema for this file (for editors)."
            }),
        );
    }
    if let Some(root) = schema.as_object_mut() {
        root.insert("title".to_owned(), Value::from("Maze settings"));
    }
    schema
}

/// Writes the settings schema to `<data_dir>/schemas/settings.json` and returns the
/// path. The file is only rewritten when its content changed, via a temporary file and
/// a rename, so editors never see a half-written schema.
pub fn write_settings_schema(data_dir: &Path) -> io::Result<PathBuf> {
    let dir = data_dir.join("schemas");
    let path = dir.join("settings.json");
    let mut json =
        serde_json::to_string_pretty(&settings_json_schema()).map_err(io::Error::other)?;
    json.push('\n');

    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == json) {
        return Ok(path);
    }
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".settings.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, json)?;
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(path)
}

/// Sets `additionalProperties: false` on every object schema with `properties` that
/// doesn't say otherwise. Maps (`accounts`, …) use `additionalProperties` with a
/// schema and are left alone.
fn close_objects(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("properties") && !map.contains_key("additionalProperties") {
                map.insert("additionalProperties".to_owned(), Value::Bool(false));
            }
            map.values_mut().for_each(close_objects);
        }
        Value::Array(items) => items.iter_mut().for_each(close_objects),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_describes_settings() {
        let schema = settings_json_schema();
        let properties = schema["properties"].as_object().unwrap();
        for key in [
            "$schema",
            "theme",
            "layout",
            "agent_panel",
            "thread_list",
            "folder_pane",
            "quick_filter",
            "reader",
            "compose",
            "spellcheck",
            "accounts",
            "agent",
            "agent_sandbox",
            "auto_update",
            "update_channel",
            "auto_update_install",
            "notifications",
            "log_level",
        ] {
            assert!(properties.contains_key(key), "missing {key}");
        }
        assert_eq!(schema["additionalProperties"], Value::Bool(false));
        // Doc comments become descriptions.
        assert!(
            properties["auto_update"]["description"]
                .as_str()
                .unwrap()
                .contains("updates")
        );
        // The range of the undo-send delay is in the schema.
        let undo = &schema["$defs"]["UndoSendSeconds"];
        assert_eq!(undo["maximum"], 30);
    }

    #[test]
    fn every_key_of_the_defaults_is_in_the_schema() {
        let schema = settings_json_schema();
        let defaults = crate::load::parse_jsonc(crate::DEFAULT_SETTINGS).unwrap();
        for key in defaults.as_object().unwrap().keys() {
            assert!(schema["properties"].get(key).is_some(), "{key}");
        }
    }

    #[test]
    fn writes_schema_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_settings_schema(dir.path()).unwrap();
        assert_eq!(path, dir.path().join("schemas/settings.json"));
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written, settings_json_schema());

        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        write_settings_schema(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        // No temporary files left behind.
        assert_eq!(
            std::fs::read_dir(dir.path().join("schemas"))
                .unwrap()
                .count(),
            1
        );
    }
}

//! Maze settings (PLAN §4.9, CONCEPT §16).
//!
//! - **Defaults** live in `assets/settings/default.jsonc`, compiled in
//!   ([`DEFAULT_SETTINGS`]).
//! - **User settings** are in `settings.json` in the config directory
//!   (`~/.config/mazemail/settings.json`, see [`Paths`]). The file is JSONC (comments
//!   and trailing commas) and only needs the keys that differ from the defaults.
//! - [`load_settings`] merges the user file over the defaults into typed [`Settings`].
//!   Unknown keys are warnings, invalid values are errors with a JSON path that fall
//!   back to the default for that key ([`Diagnostic`]); loading always yields usable
//!   settings.
//! - [`write_settings_schema`] writes the JSON Schema to
//!   `<data_dir>/schemas/settings.json` for editor autocompletion.
//! - [`SettingsWatcher`] reloads on change and publishes over a `tokio::sync::watch`
//!   channel, which works without a Tokio runtime, so the GPUI layer (`ui`) can
//!   consume it directly.
//! - [`write_setting`] changes one key of the user file and keeps its comments and
//!   formatting (Maze writes the favorite folders, `folder_pane.favorites`);
//!   [`write_settings_file`] saves the whole file from Maze's settings editor.
//! - A file with a syntax error doesn't reset anything: the watcher keeps the last
//!   good settings and publishes the diagnostics (with line numbers) for the editor.
//!
//! This crate is layer L1 and has no GPUI dependency: `auth`, `sync`, `auto_update`,
//! `agent` and `mazemail --headless` all read settings.
//!
//! JSONC is parsed with `jsonc-parser` (dprint/Deno, maintained, serde support): it is
//! exactly JSONC as editors understand it, whereas JSON5 parsers accept more syntax
//! (unquoted keys, single quotes) that schema-aware editors would flag.

mod content;
mod edit;
mod load;
mod paths;
mod schema;
mod watcher;

pub use content::*;
pub use edit::{
    EditError, USER_SETTINGS_TEMPLATE, read_or_create_settings_file, write_setting,
    write_settings_file,
};
pub use load::{
    DEFAULT_SETTINGS, Diagnostic, DiagnosticKind, LoadedSettings, default_settings, load_settings,
    parse_settings,
};
pub use paths::{
    CONFIG_DIR_ENV, DATA_DIR_ENV, Paths, PathsError, SOCKET_FILE_NAME, SUN_PATH_CAPACITY,
    SocketPath, socket_path,
};
pub use schema::{settings_json_schema, write_settings_schema};
pub use watcher::{DEFAULT_DEBOUNCE, SettingsWatcher, WatchError};

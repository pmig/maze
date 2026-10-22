//! The Zed theme file format (`$schema` v0.2.0), read with our own serde structs.
//!
//! A theme file is a *family* (for example "One") holding one or more themes, each with
//! an appearance and a flat `style` map from dotted keys (`"panel.background"`) to
//! `#rrggbbaa` colors, plus the `players` and `syntax` sections. Only the format is
//! shared with Zed; none of Zed's (GPL) theme code is used.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The schema version this module understands.
pub const SCHEMA_V0_2_0: &str = "https://zed.dev/schema/themes/v0.2.0.json";

/// A theme file: a named family of themes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThemeFamilyContent {
    /// The JSON schema the file declares, if any.
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    /// The family name, e.g. "One".
    pub name: String,
    /// Who made the themes.
    #[serde(default)]
    pub author: Option<String>,
    /// The themes in this family, usually a dark and a light one.
    pub themes: Vec<ThemeContent>,
}

impl ThemeFamilyContent {
    /// The first theme with the given appearance.
    pub fn theme(&self, appearance: AppearanceContent) -> Option<&ThemeContent> {
        self.themes.iter().find(|t| t.appearance == appearance)
    }
}

/// One theme of a family.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThemeContent {
    /// The theme name, e.g. "One Dark".
    pub name: String,
    /// Whether the theme is light or dark.
    pub appearance: AppearanceContent,
    /// Colors and styles.
    pub style: ThemeStyleContent,
}

/// The appearance of a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceContent {
    /// Dark text on a light background.
    Light,
    /// Light text on a dark background.
    Dark,
}

/// The `style` object of a theme.
///
/// Zed keeps adding color keys, so the colors are kept as a map rather than a struct
/// with a field per key: unknown keys survive, and a theme written for a newer Zed
/// still loads.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ThemeStyleContent {
    /// Collaborator colors. The first entry is the local user (cursor, selection).
    #[serde(default)]
    pub players: Vec<PlayerColorContent>,
    /// Syntax highlight styles by capture name (`"keyword"`, `"string"`, …).
    #[serde(default)]
    pub syntax: BTreeMap<String, HighlightStyleContent>,
    /// Every other key: UI colors such as `"border"` or `"text.muted"`. Values are
    /// usually color strings; `null` and non-string values are kept but ignored.
    #[serde(flatten)]
    pub colors: BTreeMap<String, serde_json::Value>,
}

impl ThemeStyleContent {
    /// The color string for a style key, if the theme sets one.
    pub fn color(&self, key: &str) -> Option<&str> {
        self.colors.get(key).and_then(serde_json::Value::as_str)
    }

    /// The local user's player colors.
    pub fn local_player(&self) -> Option<&PlayerColorContent> {
        self.players.first()
    }
}

/// Colors of one collaborator.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PlayerColorContent {
    /// Cursor color.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Avatar and label background.
    #[serde(default)]
    pub background: Option<String>,
    /// Text selection background.
    #[serde(default)]
    pub selection: Option<String>,
}

/// A syntax highlight style.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct HighlightStyleContent {
    /// Text color.
    #[serde(default)]
    pub color: Option<String>,
    /// Background color.
    #[serde(default)]
    pub background_color: Option<String>,
    /// `"normal"`, `"italic"` or `"oblique"`.
    #[serde(default)]
    pub font_style: Option<String>,
    /// CSS-style weight, 100–900.
    #[serde(default)]
    pub font_weight: Option<f32>,
}

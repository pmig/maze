//! The theme bridge: Zed theme JSON mapped onto gpui-kit's theme.
//!
//! Maze reads themes in Zed's format (CONCEPT §8.1), so existing Zed themes work. The
//! file is parsed with the structs in [`zed`], and each theme is mapped onto a
//! gpui-kit [`ThemeConfig`] through the table in [`COLOR_MAP`]. gpui-kit derives every
//! color the table leaves out (hover and active shades, chart colors, …).
//!
//! The theme's editor colors and `syntax` styles become gpui-kit's highlight theme
//! ([`highlight_style`]), so code editors such as the `settings.json` tab use the same
//! colors as the rest of Maze instead of gpui-kit's built-in highlight theme.
//!
//! The bundled default is Zed's "One" family (One Dark / One Light, MIT licensed, see
//! `assets/themes/LICENSE-one`). The window follows the system light/dark appearance
//! through [`observe_system_appearance`], unless the `theme.mode` setting forces one.
//!
//! [`apply_choice`] applies the `theme` setting: the light and dark theme by name,
//! looked up in the bundled family and in Zed theme files (`*.json`) in the user's
//! `themes/` directory, like Zed's `~/.config/zed/themes`.

pub mod zed;

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{App, Global, Rgba, SharedString, Subscription, Window, WindowAppearance};
use gpui_kit::component::highlighter::HighlightThemeStyle;
use gpui_kit::component::{Theme, ThemeConfig, ThemeConfigColors, ThemeMode};
use serde_json::{Map, Value};

use self::zed::{AppearanceContent, ThemeContent, ThemeFamilyContent};

/// The bundled default theme family (Zed's One Dark / One Light).
pub const DEFAULT_THEME_FAMILY: &str = include_str!("../../../../assets/themes/one.json");

/// Base font size of the UI in pixels. gpui-kit defaults to 16 px; a mail client needs
/// Thunderbird's density.
const UI_FONT_SIZE: f32 = 14.;
/// Corner radius of controls, close to Zed's.
const UI_RADIUS: usize = 4;
const UI_RADIUS_LG: usize = 6;

/// Errors while loading a theme file.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    /// The file isn't valid Zed theme JSON.
    #[error("invalid theme file: {0}")]
    Parse(#[from] serde_json::Error),
    /// The family has neither a light nor a dark theme.
    #[error("theme family `{0}` contains no themes")]
    Empty(String),
}

/// Where a gpui-kit color comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSource {
    /// The first of these Zed style keys that the theme sets.
    Style(&'static [&'static str]),
    /// The local player's cursor color.
    PlayerCursor,
    /// The local player's selection color.
    PlayerSelection,
}

use ColorSource::{PlayerCursor, PlayerSelection, Style};

/// How each gpui-kit theme color (left, by its gpui-kit JSON key) is taken from a Zed
/// theme (right). Keys gpui-kit knows but this table leaves out get gpui-kit's derived
/// fallbacks.
///
/// The choices follow how Zed itself paints the same surfaces: content areas use
/// `editor.background`, side panes `panel.background`, lists highlight with the
/// `ghost_element.*` colors, and `text.accent` is the accent color.
pub const COLOR_MAP: &[(&str, ColorSource)] = &[
    ("background", Style(&["editor.background", "background"])),
    ("foreground", Style(&["text", "editor.foreground"])),
    ("border", Style(&["border"])),
    ("input.border", Style(&["border", "border.variant"])),
    ("ring", Style(&["border.focused"])),
    ("muted.background", Style(&["surface.background"])),
    ("muted.foreground", Style(&["text.muted"])),
    (
        "accent.background",
        Style(&["ghost_element.hover", "element.hover"]),
    ),
    ("accent.foreground", Style(&["text"])),
    ("primary.background", Style(&["text.accent"])),
    ("primary.foreground", Style(&["editor.background"])),
    ("secondary.background", Style(&["element.background"])),
    ("secondary.hover.background", Style(&["element.hover"])),
    ("secondary.active.background", Style(&["element.active"])),
    ("secondary.foreground", Style(&["text"])),
    (
        "popover.background",
        Style(&["elevated_surface.background"]),
    ),
    ("popover.foreground", Style(&["text"])),
    ("list.background", Style(&["panel.background"])),
    ("list.hover.background", Style(&["ghost_element.hover"])),
    (
        "list.active.background",
        Style(&["ghost_element.selected", "element.selected"]),
    ),
    ("list.active.border", Style(&["border.focused"])),
    ("list.head.background", Style(&["panel.background"])),
    ("table.background", Style(&["editor.background"])),
    ("table.head.background", Style(&["panel.background"])),
    ("table.head.foreground", Style(&["text.muted"])),
    ("table.hover.background", Style(&["ghost_element.hover"])),
    (
        "table.active.background",
        Style(&["ghost_element.selected", "element.selected"]),
    ),
    ("table.active.border", Style(&["border.focused"])),
    ("table.row.border", Style(&["border.variant"])),
    ("table.even.background", Style(&["editor.background"])),
    ("sidebar.background", Style(&["panel.background"])),
    ("sidebar.foreground", Style(&["text"])),
    ("sidebar.border", Style(&["border"])),
    (
        "sidebar.accent.background",
        Style(&["ghost_element.selected"]),
    ),
    ("sidebar.accent.foreground", Style(&["text"])),
    ("tab_bar.background", Style(&["tab_bar.background"])),
    ("tab.background", Style(&["tab.inactive_background"])),
    ("tab.active.background", Style(&["tab.active_background"])),
    ("tab.foreground", Style(&["text.muted"])),
    ("tab.active.foreground", Style(&["text"])),
    ("title_bar.background", Style(&["title_bar.background"])),
    ("title_bar.border", Style(&["border"])),
    ("status_bar.background", Style(&["status_bar.background"])),
    ("status_bar.border", Style(&["border"])),
    (
        "scrollbar.background",
        Style(&["scrollbar.track.background"]),
    ),
    (
        "scrollbar.thumb.background",
        Style(&["scrollbar.thumb.background"]),
    ),
    (
        "scrollbar.thumb.hover.background",
        Style(&["scrollbar.thumb.hover_background"]),
    ),
    ("selection.background", PlayerSelection),
    ("caret", PlayerCursor),
    ("link", Style(&["text.accent"])),
    ("link.hover", Style(&["link_text.hover", "text.accent"])),
    ("drop_target.background", Style(&["drop_target.background"])),
    ("drag.border", Style(&["border.focused"])),
    ("danger.background", Style(&["error"])),
    ("success.background", Style(&["success", "created"])),
    ("warning.background", Style(&["warning", "modified"])),
    ("info.background", Style(&["info"])),
    ("skeleton.background", Style(&["element.background"])),
    ("group_box.background", Style(&["surface.background"])),
    ("window.border", Style(&["border"])),
    ("progress.bar.background", Style(&["text.accent"])),
    ("slider.background", Style(&["text.accent"])),
    ("base.red", Style(&["terminal.ansi.red", "error"])),
    ("base.green", Style(&["terminal.ansi.green", "success"])),
    ("base.blue", Style(&["terminal.ansi.blue", "text.accent"])),
    ("base.yellow", Style(&["terminal.ansi.yellow", "warning"])),
    ("base.magenta", Style(&["terminal.ansi.magenta"])),
    ("base.cyan", Style(&["terminal.ansi.cyan", "info"])),
];

/// Resolves one [`ColorSource`] against a theme, skipping values that aren't colors.
fn resolve(theme: &ThemeContent, source: ColorSource) -> Option<&str> {
    let player = theme.style.local_player();
    let candidates: Vec<Option<&str>> = match source {
        Style(keys) => keys.iter().map(|key| theme.style.color(key)).collect(),
        PlayerCursor => vec![player.and_then(|p| p.cursor.as_deref())],
        PlayerSelection => vec![player.and_then(|p| p.selection.as_deref())],
    };
    candidates
        .into_iter()
        .flatten()
        .find(|value| is_color(value))
}

/// Whether gpui can parse `value` as a hex color (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`).
pub fn is_color(value: &str) -> bool {
    Rgba::try_from(value).is_ok()
}

/// Zed's status keys that gpui-kit's highlight theme reads under the same names.
const STATUS_KEYS: &[&str] = &[
    "error",
    "error.background",
    "error.border",
    "warning",
    "warning.background",
    "warning.border",
    "info",
    "info.background",
    "info.border",
    "success",
    "success.background",
    "success.border",
    "hint",
    "hint.background",
    "hint.border",
];

/// The theme's editor colors and syntax styles as gpui-kit's highlight theme. The
/// editor colors are set field by field; status colors and syntax styles use Zed's key
/// names, which gpui-kit reads too. Invalid colors are skipped, `oblique` becomes
/// italic and weights snap to the nearest of 100…900.
pub fn highlight_style(theme: &ThemeContent) -> Option<HighlightThemeStyle> {
    let hsla = |key: &str| {
        theme
            .style
            .color(key)
            .and_then(|c| Rgba::try_from(c).ok())
            .map(gpui::Hsla::from)
    };
    let mut status = Map::new();
    for key in STATUS_KEYS {
        if let Some(color) = theme.style.color(key).filter(|c| is_color(c)) {
            status.insert((*key).to_owned(), Value::String(color.to_owned()));
        }
    }
    let mut syntax = Map::new();
    for (capture, highlight) in &theme.style.syntax {
        let mut entry = Map::new();
        if let Some(color) = highlight.color.as_deref().filter(|c| is_color(c)) {
            entry.insert("color".into(), Value::String(color.to_owned()));
        }
        match highlight.font_style.as_deref() {
            Some("italic" | "oblique") => {
                entry.insert("font_style".into(), Value::String("italic".into()));
            }
            Some("normal") => {
                entry.insert("font_style".into(), Value::String("normal".into()));
            }
            _ => {}
        }
        if let Some(weight) = highlight.font_weight {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let weight = ((weight / 100.).round().clamp(1., 9.) as u16) * 100;
            entry.insert("font_weight".into(), Value::from(weight));
        }
        if !entry.is_empty() {
            syntax.insert(capture.clone(), Value::Object(entry));
        }
    }
    let syntax = match serde_json::from_value(Value::Object(syntax)) {
        Ok(syntax) => syntax,
        Err(error) => {
            tracing::warn!(%error, theme = %theme.name, "theme syntax mapping rejected");
            return None;
        }
    };
    Some(HighlightThemeStyle {
        editor_background: hsla("editor.background"),
        editor_foreground: hsla("editor.foreground"),
        editor_active_line: hsla("editor.active_line.background"),
        editor_line_number: hsla("editor.line_number"),
        editor_active_line_number: hsla("editor.active_line_number"),
        editor_invisible: hsla("editor.invisible"),
        editor_gutter_background: hsla("editor.gutter.background"),
        status: serde_json::from_value(Value::Object(status)).unwrap_or_default(),
        syntax,
    })
}

/// Maps one Zed theme onto a gpui-kit theme configuration.
pub fn theme_config(theme: &ThemeContent) -> ThemeConfig {
    let mut colors = Map::new();
    for (kit_key, source) in COLOR_MAP {
        if let Some(value) = resolve(theme, *source) {
            colors.insert((*kit_key).to_owned(), Value::String(value.to_owned()));
        }
    }
    let colors: ThemeConfigColors = match serde_json::from_value(Value::Object(colors)) {
        Ok(colors) => colors,
        Err(error) => {
            // Only possible if a key in COLOR_MAP isn't a gpui-kit key; a unit test
            // guards against that.
            tracing::warn!(%error, theme = %theme.name, "theme color mapping rejected");
            ThemeConfigColors::default()
        }
    };

    ThemeConfig {
        name: SharedString::from(theme.name.clone()),
        mode: match theme.appearance {
            AppearanceContent::Light => ThemeMode::Light,
            AppearanceContent::Dark => ThemeMode::Dark,
        },
        font_size: Some(UI_FONT_SIZE),
        radius: Some(UI_RADIUS),
        radius_lg: Some(UI_RADIUS_LG),
        shadow: Some(false),
        colors,
        highlight: highlight_style(theme),
        ..ThemeConfig::default()
    }
}

/// The light and dark theme Maze switches between, as gpui-kit configurations.
#[derive(Debug, Clone)]
pub struct ThemePair {
    /// The family name, e.g. "One".
    pub family: SharedString,
    /// Used when the system appearance is light.
    pub light: Rc<ThemeConfig>,
    /// Used when the system appearance is dark.
    pub dark: Rc<ThemeConfig>,
}

impl ThemePair {
    /// Parses a Zed theme family file and maps its light and dark themes. A family
    /// with only one appearance uses that theme for both.
    pub fn from_json(json: &str) -> Result<Self, ThemeError> {
        let family: ThemeFamilyContent = serde_json::from_str(json)?;
        Self::from_family(&family)
    }

    /// Maps an already parsed theme family.
    pub fn from_family(family: &ThemeFamilyContent) -> Result<Self, ThemeError> {
        let light = family.theme(AppearanceContent::Light);
        let dark = family.theme(AppearanceContent::Dark);
        let (light, dark) = match (light, dark) {
            (Some(light), Some(dark)) => (light, dark),
            (Some(only), None) | (None, Some(only)) => (only, only),
            (None, None) => return Err(ThemeError::Empty(family.name.clone())),
        };
        Ok(Self {
            family: SharedString::from(family.name.clone()),
            light: Rc::new(theme_config(light)),
            dark: Rc::new(theme_config(dark)),
        })
    }
}

/// The installed theme family, kept so a later appearance change can re-apply it.
struct ActiveThemePair(ThemePair);

impl Global for ActiveThemePair {}

/// Light or dark: follow the system, or always one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Appearance {
    /// Follow the system appearance.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// The `theme` setting: the appearance and the theme used in each mode, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeChoice {
    /// System, light or dark.
    pub appearance: Appearance,
    /// The theme in light mode, e.g. "One Light".
    pub light: String,
    /// The theme in dark mode, e.g. "One Dark".
    pub dark: String,
    /// Where user themes live (`<config>/themes`), if anywhere.
    pub themes_dir: Option<PathBuf>,
}

/// The applied [`ThemeChoice`]: re-applying the same one does nothing, and a forced
/// appearance stops [`observe_system_appearance`] from switching.
struct AppliedChoice(ThemeChoice);

impl Global for AppliedChoice {}

/// Every theme Maze can use: the bundled family, then the families in `themes_dir`
/// (files that don't parse are skipped with a warning).
pub fn available_themes(themes_dir: Option<&Path>) -> Vec<ThemeContent> {
    let mut themes = Vec::new();
    match serde_json::from_str::<ThemeFamilyContent>(DEFAULT_THEME_FAMILY) {
        Ok(family) => themes.extend(family.themes),
        Err(error) => tracing::error!(%error, "bundled theme failed to load"),
    }
    let Some(dir) = themes_dir else {
        return themes;
    };
    let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            tracing::warn!(%error, "cannot read the themes directory");
            Vec::new()
        }
    };
    files.sort();
    for file in files {
        let parsed = std::fs::read_to_string(&file)
            .map_err(|e| e.to_string())
            .and_then(|json| {
                serde_json::from_str::<ThemeFamilyContent>(&json).map_err(|e| e.to_string())
            });
        match parsed {
            Ok(family) => themes.extend(family.themes),
            Err(error) => tracing::warn!(%error, file = %file.display(), "skipping a theme file"),
        }
    }
    themes
}

/// The light and dark theme a choice names, falling back to the bundled One Light /
/// One Dark (with a warning) for names that don't exist. Names compare ignoring case.
pub fn resolve_choice(choice: &ThemeChoice, themes: &[ThemeContent]) -> Option<ThemePair> {
    let find = |name: &str, appearance: AppearanceContent, fallback: &str| {
        let by_name = |n: &str| {
            themes
                .iter()
                .find(|t| t.name.trim().eq_ignore_ascii_case(n.trim()))
        };
        by_name(name).or_else(|| {
            tracing::warn!(theme = name, "no such theme; using the default");
            by_name(fallback).or_else(|| themes.iter().find(|t| t.appearance == appearance))
        })
    };
    let light = find(&choice.light, AppearanceContent::Light, "One Light")?;
    let dark = find(&choice.dark, AppearanceContent::Dark, "One Dark")?;
    Some(ThemePair {
        family: SharedString::from(format!("{} / {}", light.name, dark.name)),
        light: Rc::new(theme_config(light)),
        dark: Rc::new(theme_config(dark)),
    })
}

/// Applies the `theme` setting (see the [module docs](self)). Does nothing if the same
/// choice is already applied.
pub fn apply_choice(choice: ThemeChoice, cx: &mut App) {
    if cx
        .try_global::<AppliedChoice>()
        .is_some_and(|applied| applied.0 == choice)
    {
        return;
    }
    let themes = available_themes(choice.themes_dir.as_deref());
    let Some(pair) = resolve_choice(&choice, &themes) else {
        return;
    };
    cx.set_global(AppliedChoice(choice));
    install(pair, cx);
}

/// The appearance the setting forces, if any.
fn forced_mode(cx: &App) -> Option<ThemeMode> {
    match cx.try_global::<AppliedChoice>().map(|c| c.0.appearance) {
        Some(Appearance::Light) => Some(ThemeMode::Light),
        Some(Appearance::Dark) => Some(ThemeMode::Dark),
        Some(Appearance::System) | None => None,
    }
}

/// Installs the bundled theme family and applies the variant matching the system
/// appearance. Call after `gpui_kit::init`.
pub fn init(cx: &mut App) {
    match ThemePair::from_json(DEFAULT_THEME_FAMILY) {
        Ok(pair) => install(pair, cx),
        // The bundled file is covered by tests, so this only happens with a broken
        // build; gpui-kit's own default theme stays active.
        Err(error) => tracing::error!(%error, "bundled theme failed to load"),
    }
}

/// Makes `pair` the active theme family and applies the variant for the current
/// system appearance.
pub fn install(pair: ThemePair, cx: &mut App) {
    tracing::debug!(family = %pair.family, "installing theme family");
    let (light, dark) = (pair.light.clone(), pair.dark.clone());
    cx.set_global(ActiveThemePair(pair));
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
    let mode = forced_mode(cx).unwrap_or_else(|| mode_for(cx.window_appearance()));
    Theme::change(mode, None, cx);
}

/// The theme mode for a system appearance.
pub fn mode_for(appearance: WindowAppearance) -> ThemeMode {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => ThemeMode::Dark,
        WindowAppearance::Light | WindowAppearance::VibrantLight => ThemeMode::Light,
    }
}

/// Keeps the theme in step with the window's light/dark appearance. Hold on to the
/// returned subscription for as long as the window lives.
pub fn observe_system_appearance(window: &Window) -> Subscription {
    window.observe_window_appearance(|window, cx| {
        if forced_mode(cx).is_some() {
            return;
        }
        let mode = mode_for(window.appearance());
        if Theme::global(cx).mode != mode {
            Theme::change(mode, Some(window), cx);
        }
    })
}

/// The name of the active theme family, if Maze installed one.
pub fn active_family(cx: &App) -> Option<SharedString> {
    cx.try_global::<ActiveThemePair>()
        .map(|pair| pair.0.family.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family() -> ThemeFamilyContent {
        serde_json::from_str(DEFAULT_THEME_FAMILY).unwrap()
    }

    #[test]
    fn bundled_family_parses() {
        let family = family();
        assert_eq!(family.name, "One");
        assert_eq!(family.schema.as_deref(), Some(zed::SCHEMA_V0_2_0));
        assert!(family.theme(AppearanceContent::Dark).is_some());
        assert!(family.theme(AppearanceContent::Light).is_some());
        let dark = family.theme(AppearanceContent::Dark).unwrap();
        assert_eq!(dark.style.color("text"), Some("#dce0e5ff"));
        // `null` values stay in the map but aren't colors.
        assert_eq!(dark.style.color("panel.focused_border"), None);
        assert!(!dark.style.syntax.is_empty());
    }

    #[test]
    fn every_mapped_key_is_a_gpui_kit_key() {
        for (key, _) in COLOR_MAP {
            let mut map = Map::new();
            map.insert((*key).to_owned(), Value::String("#ff0000ff".into()));
            let parsed: ThemeConfigColors =
                serde_json::from_value(Value::Object(map.clone())).unwrap();
            // A known key round-trips; an unknown key would be dropped by serde.
            let back = serde_json::to_value(&parsed).unwrap();
            assert_eq!(
                back.get(*key),
                Some(&Value::String("#ff0000ff".into())),
                "`{key}` is not a gpui-kit theme color key"
            );
        }
    }

    #[test]
    fn maps_one_dark_onto_gpui_kit() {
        let family = family();
        let dark = theme_config(family.theme(AppearanceContent::Dark).unwrap());
        assert_eq!(dark.mode, ThemeMode::Dark);
        assert_eq!(dark.name.as_ref(), "One Dark");
        assert_eq!(dark.colors.background.as_deref(), Some("#282c33ff"));
        assert_eq!(dark.colors.foreground.as_deref(), Some("#dce0e5ff"));
        assert_eq!(dark.colors.primary.as_deref(), Some("#74ade8ff"));
        assert_eq!(dark.colors.sidebar.as_deref(), Some("#2f343eff"));
        assert_eq!(dark.colors.selection.as_deref(), Some("#74ade83d"));
        assert_eq!(dark.colors.caret.as_deref(), Some("#74ade8ff"));

        let light = theme_config(family.theme(AppearanceContent::Light).unwrap());
        assert_eq!(light.mode, ThemeMode::Light);
        assert_eq!(light.colors.background.as_deref(), Some("#fafafaff"));
    }

    #[test]
    fn falls_back_through_keys_and_skips_invalid_colors() {
        let json = r##"{
            "name": "Test",
            "themes": [{
                "name": "Test Dark",
                "appearance": "dark",
                "style": {
                    "editor.background": "not a color",
                    "background": "#101010ff",
                    "text": null
                }
            }]
        }"##;
        let pair = ThemePair::from_json(json).unwrap();
        // Only a dark theme: it serves both appearances.
        assert_eq!(pair.light.name, pair.dark.name);
        assert_eq!(pair.dark.colors.background.as_deref(), Some("#101010ff"));
        assert_eq!(pair.dark.colors.foreground, None);
    }

    #[test]
    fn rejects_empty_families_and_bad_json() {
        assert!(matches!(
            ThemePair::from_json(r#"{"name":"Nothing","themes":[]}"#),
            Err(ThemeError::Empty(name)) if name == "Nothing"
        ));
        assert!(matches!(
            ThemePair::from_json("{"),
            Err(ThemeError::Parse(_))
        ));
    }

    #[test]
    fn appearance_maps_to_mode() {
        assert_eq!(mode_for(WindowAppearance::Dark), ThemeMode::Dark);
        assert_eq!(mode_for(WindowAppearance::VibrantDark), ThemeMode::Dark);
        assert_eq!(mode_for(WindowAppearance::Light), ThemeMode::Light);
        assert_eq!(mode_for(WindowAppearance::VibrantLight), ThemeMode::Light);
    }

    #[gpui::test]
    fn installs_into_the_global_theme(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx);
            assert_eq!(active_family(cx).as_deref(), Some("One"));
            let theme = Theme::global(cx);
            assert!(theme.theme_name().starts_with("One"));
            assert_eq!(theme.font_size, gpui::px(UI_FONT_SIZE));
            // Code editors (the settings tab) use the theme's editor colors.
            let family = family();
            let mode = theme.mode;
            let expected = family
                .theme(if mode.is_dark() {
                    AppearanceContent::Dark
                } else {
                    AppearanceContent::Light
                })
                .and_then(|t| t.style.color("editor.background"))
                .map(|c| gpui::Hsla::from(Rgba::try_from(c).unwrap()));
            assert_eq!(theme.highlight_theme.style.editor_background, expected);
        });
    }

    fn choice(light: &str, dark: &str, dir: Option<&Path>) -> ThemeChoice {
        ThemeChoice {
            appearance: Appearance::System,
            light: light.into(),
            dark: dark.into(),
            themes_dir: dir.map(Path::to_path_buf),
        }
    }

    #[test]
    fn themes_come_from_the_bundle_and_the_themes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("graphite.json"),
            r##"{ "name": "Graphite", "author": "me", "themes": [
                { "name": "Graphite Dark", "appearance": "dark",
                  "style": { "editor.background": "#18181bff", "text": "#f4f4f5ff" } } ] }"##,
        )
        .unwrap();
        std::fs::write(tmp.path().join("broken.json"), "{ nope").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "ignored").unwrap();
        let themes = available_themes(Some(tmp.path()));
        let names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"One Light") && names.contains(&"Graphite Dark"));

        // By name, ignoring case; the light theme stays the bundled one.
        let pair = resolve_choice(&choice("One Light", "graphite dark", None), &themes).unwrap();
        assert_eq!(pair.dark.name.as_ref(), "Graphite Dark");
        assert_eq!(pair.light.name.as_ref(), "One Light");
        // Unknown names fall back to the defaults.
        let pair = resolve_choice(&choice("Nope", "Also Nope", None), &themes).unwrap();
        assert_eq!(
            (pair.light.name.as_ref(), pair.dark.name.as_ref()),
            ("One Light", "One Dark")
        );
        // A missing directory is just the bundle.
        assert_eq!(available_themes(Some(&tmp.path().join("missing"))).len(), 2);
    }

    #[gpui::test]
    fn a_forced_appearance_wins_over_the_system(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx);
            apply_choice(
                ThemeChoice {
                    appearance: Appearance::Dark,
                    ..choice("One Light", "One Dark", None)
                },
                cx,
            );
            assert_eq!(Theme::global(cx).mode, ThemeMode::Dark);
            assert_eq!(Theme::global(cx).theme_name().as_ref(), "One Dark");
            apply_choice(
                ThemeChoice {
                    appearance: Appearance::Light,
                    ..choice("One Light", "One Dark", None)
                },
                cx,
            );
            assert_eq!(Theme::global(cx).mode, ThemeMode::Light);
            assert_eq!(Theme::global(cx).theme_name().as_ref(), "One Light");
        });
    }

    #[test]
    fn maps_editor_colors_and_syntax_onto_the_highlight_theme() {
        let family = family();
        let dark = family.theme(AppearanceContent::Dark).unwrap();
        let style = highlight_style(dark).expect("One Dark maps");
        let color = |key: &str| {
            dark.style
                .color(key)
                .map(|c| gpui::Hsla::from(Rgba::try_from(c).unwrap()))
        };
        assert_eq!(style.editor_background, color("editor.background"));
        assert_eq!(style.editor_foreground, color("editor.foreground"));
        assert_eq!(style.editor_line_number, color("editor.line_number"));
        assert!(style.syntax.string.is_some());
        assert!(style.syntax.property.is_some(), "JSON keys are properties");
        assert!(style.syntax.comment.is_some());
        assert!(theme_config(dark).highlight.is_some());

        // Odd values are tolerated: oblique, off-scale weights, broken colors.
        let theme: ThemeContent = serde_json::from_value(serde_json::json!({
            "name": "Odd",
            "appearance": "light",
            "style": {
                "editor.background": "not a color",
                "syntax": {
                    "keyword": { "color": "#ff0000", "font_style": "oblique", "font_weight": 650.0 },
                    "string": { "color": "nope" }
                }
            }
        }))
        .unwrap();
        let style = highlight_style(&theme).expect("still maps");
        assert_eq!(style.editor_background, None);
        assert!(style.syntax.keyword.is_some());
    }
}

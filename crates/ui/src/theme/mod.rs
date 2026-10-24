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
//! through [`observe_system_appearance`].

pub mod zed;

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
    let mode = mode_for(cx.window_appearance());
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

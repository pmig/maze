//! Maze's UI foundation (layer L3), shared by every GPUI crate.
//!
//! - [`theme`]: Zed theme JSON mapped onto gpui-kit's theme, following the system
//!   light/dark appearance.
//! - [`keymap`]: Zed-format keymap files turned into GPUI key bindings.
//! - [`gpui_tokio`]: run Tokio futures from GPUI ([`Tokio::spawn`]).
//! - [`assets`]: the embedded icon set.
//! - [`components`]: small shared components.
//! - [`panes`]: tabs and splits, the way Zed does them (ADR-018).
//! - [`names`]: random two-word names (`brave-otter`) for new drafts.
//!
//! The component library is gpui-kit (PLAN ADR-002), re-exported as [`kit`] so crates
//! above use the same version.

pub mod assets;
pub mod components;
pub mod gpui_tokio;
pub mod keymap;
pub mod names;
pub mod panes;
pub mod theme;

pub use gpui_kit as kit;
pub use gpui_tokio::Tokio;

/// Initializes gpui-kit and installs Maze's theme. Call once at startup, before
/// opening windows. The Tokio bridge is set up separately with [`gpui_tokio::init`].
pub fn init(cx: &mut gpui::App) {
    gpui_kit::init(cx);
    theme::init(cx);
}

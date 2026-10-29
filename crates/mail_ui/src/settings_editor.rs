//! Maze's settings in a tab: `settings.json` in a plain-text JSON editor (gpui-kit's
//! code editor, highlighted as JSON), and the built-in defaults read-only beside it.
//!
//! - **Open** ("Maze: Open Settings", `cmd-,`, "Settings…" in the Maze menu) shows the
//!   user's `settings.json` in a tab of the active pane, creating it from a short
//!   template if it doesn't exist. "Maze: Open Default Settings" shows `default.jsonc`
//!   read-only, for reference. An open tab is activated instead of opening another.
//! - **Save** (`cmd-s`) writes the file atomically with a trailing newline and has the
//!   settings reloaded right away ([`SettingsFile::on_save`]). Unsaved changes show the
//!   tab's dot, and closing asks "Save / Don't Save / Cancel".
//! - **Diagnostics** of the saved text (syntax errors, unknown keys, wrong types, each
//!   with its line) are listed in a strip below the editor. A broken file never resets
//!   anything: the settings watcher keeps the last good settings until it is fixed, and
//!   an invalid value falls back to its default on its own.
//! - **Changes on disk** (Maze writes the favorite folders itself, or another editor
//!   saves) are noticed within [`DISK_CHECK_INTERVAL`]: a buffer without unsaved edits
//!   is reloaded; one with edits asks "Reload / Keep Mine".
//!
//! The tab is kept in the layout like the others (`{"kind": "settings"}`, with
//! `"defaults": true` for the defaults), never the file's content.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, PromptLevel, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    prelude::FluentBuilder as _, relative,
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Editor, EditorState, InputEvent},
    v_flex,
};
use serde::{Deserialize, Serialize};
use settings::{DEFAULT_SETTINGS, Diagnostic, DiagnosticKind};
use ui::assets::IconName;
use ui::keymap::shortcut_label;
use ui::panes::{Item, ItemEvent, SerializedItem};
use ui::theme::scaled;

use crate::actions::{OpenDefaultSettings, SaveSettings};

/// The layout kind of settings tabs (`{"kind": "settings"}`).
pub const SETTINGS_KIND: &str = "settings";

/// How often an open settings file is compared with the file on disk.
pub const DISK_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// What a settings tab keeps in the layout file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerializedSettings {
    /// The built-in defaults (read-only) rather than the user's file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub defaults: bool,
}

/// What runs after the settings file was saved.
type OnSave = Rc<dyn Fn(&mut App)>;

/// The user's settings file as the window edits it: where it is, and what to do after
/// it was saved (the app reloads the settings).
#[derive(Clone)]
pub struct SettingsFile {
    path: PathBuf,
    on_save: Option<OnSave>,
}

impl fmt::Debug for SettingsFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsFile")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SettingsFile {
    /// The settings file at `path` (`Paths::settings_file`).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            on_save: None,
        }
    }

    /// Runs `reload` after every save, so the new settings apply at once
    /// (`SettingsWatcher::reload`).
    pub fn on_save(mut self, reload: impl Fn(&mut App) + 'static) -> Self {
        self.on_save = Some(Rc::new(reload));
        self
    }

    /// The file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// What the tab edits.
enum Source {
    /// The user's `settings.json`.
    File(SettingsFile),
    /// `default.jsonc`, read-only.
    Defaults,
}

/// A settings tab: the user's file, or the defaults.
pub struct SettingsEditor {
    source: Source,
    editor: Entity<EditorState>,
    /// The buffer as last loaded or saved: the tab is dirty while the text differs.
    saved_text: String,
    /// The file as last read or written (`None`: it doesn't exist), to notice changes
    /// made by others.
    disk_text: Option<String>,
    dirty: bool,
    /// The file changed on disk while the buffer had unsaved edits.
    conflict: bool,
    /// Problems of the text last loaded or saved.
    diagnostics: Vec<Diagnostic>,
    /// Reading or writing the file failed.
    error: Option<SharedString>,
    _disk_check: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl fmt::Debug for SettingsEditor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsEditor")
            .field("defaults", &self.is_defaults())
            .field("dirty", &self.dirty)
            .field("conflict", &self.conflict)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<ItemEvent> for SettingsEditor {}

/// Reads the file; `Ok(None)` when it doesn't exist.
fn read_file(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn diagnostics_of(text: &str) -> Vec<Diagnostic> {
    settings::parse_settings(Some(text)).diagnostics
}

impl SettingsEditor {
    /// The user's settings file, created from a template if it doesn't exist yet.
    pub fn open(file: SettingsFile, window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let (text, error) = match settings::read_or_create_settings_file(file.path()) {
            Ok(text) => (text, None),
            Err(err) => {
                tracing::warn!(%err, "opening the settings file");
                (String::new(), Some(err.to_string().into()))
            }
        };
        let editor = Self::new_editor(&text, false, window, cx);
        let subscriptions = vec![cx.subscribe_in(&editor, window, Self::on_editor_event)];
        let disk_check = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(DISK_CHECK_INTERVAL).await;
                let checked = this.update_in(cx, |this, window, cx| {
                    this.check_disk(window, cx);
                });
                if checked.is_err() {
                    break;
                }
            }
        });
        Self {
            diagnostics: diagnostics_of(&text),
            disk_text: error.is_none().then(|| text.clone()),
            saved_text: text,
            source: Source::File(file),
            editor,
            dirty: false,
            conflict: false,
            error,
            _disk_check: Some(disk_check),
            _subscriptions: subscriptions,
        }
    }

    /// Maze's default settings, read-only.
    pub fn defaults(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let editor = Self::new_editor(DEFAULT_SETTINGS, true, window, cx);
        Self {
            source: Source::Defaults,
            editor,
            saved_text: DEFAULT_SETTINGS.to_owned(),
            disk_text: None,
            dirty: false,
            conflict: false,
            diagnostics: Vec::new(),
            error: None,
            _disk_check: None,
            _subscriptions: Vec::new(),
        }
    }

    fn new_editor(
        text: &str,
        readonly: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<EditorState> {
        let text = text.to_owned();
        cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("json")
                .default_value(text);
            if readonly {
                state.set_readonly(true, cx);
            }
            state
        })
    }

    // ---- state accessors ------------------------------------------------------------

    /// Whether the tab shows the built-in defaults.
    pub fn is_defaults(&self) -> bool {
        matches!(self.source, Source::Defaults)
    }

    /// The edited file (`None` for the defaults).
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            Source::File(file) => Some(file.path()),
            Source::Defaults => None,
        }
    }

    /// The editor.
    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    /// The buffer's text.
    pub fn text(&self, cx: &App) -> String {
        self.editor.read(cx).value().to_string()
    }

    /// Whether the buffer has unsaved edits.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the file changed on disk while the buffer had unsaved edits (the tab
    /// asks "Reload / Keep Mine").
    pub fn has_conflict(&self) -> bool {
        self.conflict
    }

    /// The problems of the text last loaded or saved, as the strip lists them.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Why the file couldn't be read or written, if it couldn't.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    // ---- editing, saving, the disk ----------------------------------------------------

    fn on_editor_event(
        &mut self,
        _: &Entity<EditorState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            self.update_dirty(cx);
        }
    }

    fn update_dirty(&mut self, cx: &mut Context<'_, Self>) {
        let dirty = !self.is_defaults() && self.text(cx) != self.saved_text;
        if dirty != self.dirty {
            self.dirty = dirty;
            cx.emit(ItemEvent::UpdateTab);
        }
        cx.notify();
    }

    /// Writes the buffer to the file and reloads the settings (`cmd-s`). Returns
    /// whether it was written.
    pub fn save(&mut self, cx: &mut Context<'_, Self>) -> bool {
        let Source::File(file) = &self.source else {
            return false;
        };
        let file = file.clone();
        let text = self.text(cx);
        if let Err(err) = settings::write_settings_file(file.path(), &text) {
            tracing::warn!(%err, "saving the settings file");
            self.error = Some(err.to_string().into());
            cx.notify();
            return false;
        }
        self.disk_text = read_file(file.path()).ok().flatten();
        self.saved_text = text;
        self.dirty = false;
        self.conflict = false;
        self.error = None;
        self.diagnostics = diagnostics_of(&self.saved_text);
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
        if let Some(reload) = file.on_save {
            reload(cx);
        }
        true
    }

    /// Compares the file with what the tab last read or wrote (every
    /// [`DISK_CHECK_INTERVAL`]). A change reloads a buffer without unsaved edits, and
    /// makes one with edits ask "Reload / Keep Mine".
    pub fn check_disk(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let Some(path) = self.path().map(Path::to_path_buf) else {
            return;
        };
        let on_disk = match read_file(&path) {
            Ok(text) => text,
            Err(err) => {
                tracing::debug!(%err, "checking the settings file");
                return;
            }
        };
        if on_disk == self.disk_text {
            return;
        }
        self.disk_text = on_disk;
        if self.dirty {
            self.conflict = true;
            cx.notify();
        } else {
            self.load_from_disk(window, cx);
        }
    }

    /// "Reload": the buffer takes the file's text, dropping unsaved edits.
    pub fn reload_from_disk(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if let Some(path) = self.path().map(Path::to_path_buf) {
            match read_file(&path) {
                Ok(text) => self.disk_text = text,
                Err(err) => {
                    self.error = Some(format!("cannot read {}: {err}", path.display()).into());
                    cx.notify();
                    return;
                }
            }
        }
        self.load_from_disk(window, cx);
    }

    /// "Keep Mine": the buffer stays as it is; saving it replaces the file.
    pub fn keep_mine(&mut self, cx: &mut Context<'_, Self>) {
        self.conflict = false;
        cx.notify();
    }

    fn load_from_disk(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let text = self.disk_text.clone().unwrap_or_default();
        self.editor
            .update(cx, |editor, cx| editor.set_value(text.clone(), window, cx));
        self.diagnostics = diagnostics_of(&text);
        self.saved_text = text;
        self.conflict = false;
        self.error = None;
        self.update_dirty(cx);
    }

    /// Closing with unsaved edits asks first; `false` keeps the tab until the answer.
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        if !self.dirty {
            return true;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Save the changes to your settings?",
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else {
                return;
            };
            let _ = this.update_in(cx, |this, _, cx| match answer {
                0 => {
                    if this.save(cx) {
                        cx.emit(ItemEvent::Close);
                    }
                }
                1 => {
                    this.dirty = false;
                    cx.emit(ItemEvent::Close);
                }
                _ => {}
            });
        })
        .detach();
        false
    }

    // ---- rendering --------------------------------------------------------------------

    fn render_banner(&self, cx: &mut Context<'_, Self>) -> Option<gpui::AnyElement> {
        let colors = cx.theme().colors;
        if self.conflict {
            return Some(
                h_flex()
                    .id("settings-conflict")
                    .debug_selector(|| "settings-conflict".to_owned())
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(colors.border)
                    .bg(colors.warning.opacity(0.12))
                    .text_sm()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child("The file changed on disk — reload it, or keep your changes?"),
                    )
                    .child(
                        Button::new("settings-reload")
                            .debug_selector(|| "settings-reload".to_owned())
                            .small()
                            .label("Reload")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.reload_from_disk(window, cx);
                            })),
                    )
                    .child(
                        Button::new("settings-keep-mine")
                            .debug_selector(|| "settings-keep-mine".to_owned())
                            .small()
                            .ghost()
                            .label("Keep Mine")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.keep_mine(cx);
                            })),
                    )
                    .into_any_element(),
            );
        }
        if self.is_defaults() {
            return Some(
                div()
                    .id("settings-banner")
                    .debug_selector(|| "settings-banner".to_owned())
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(colors.border)
                    .text_sm()
                    .text_color(colors.muted_foreground)
                    .child(format!(
                        "Maze's default settings, read-only. Copy what you want to change \
                         into your settings ({}).",
                        shortcut_label("cmd-,", cx)
                    ))
                    .into_any_element(),
            );
        }
        Some(
            h_flex()
                .id("settings-banner")
                .debug_selector(|| "settings-banner".to_owned())
                .flex_wrap()
                .gap_x_1()
                .px_3()
                .py_1p5()
                .border_b_1()
                .border_color(colors.border)
                .text_sm()
                .text_color(colors.muted_foreground)
                .child(
                    "Your settings: only what you set here changes Maze's defaults. Every \
                     available setting, with its default and what it does, is in the",
                )
                .child(
                    Button::new("settings-open-defaults")
                        .debug_selector(|| "settings-open-defaults".to_owned())
                        .link()
                        .small()
                        .label("default settings")
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(OpenDefaultSettings), cx);
                        }),
                )
                .child("— copy keys from there into this file.")
                .into_any_element(),
        )
    }

    fn render_diagnostics(&self, cx: &mut Context<'_, Self>) -> Option<gpui::AnyElement> {
        if self.diagnostics.is_empty() && self.error.is_none() {
            return None;
        }
        let colors = cx.theme().colors;
        let file_ignored = self
            .diagnostics
            .iter()
            .any(|d| d.is_error() && d.path.is_empty());
        let rows = self.diagnostics.iter().map(|diagnostic| {
            let color = if diagnostic.is_error() {
                colors.danger
            } else {
                colors.warning
            };
            let line = diagnostic
                .line
                .map(|line| format!("Line {line}"))
                .unwrap_or_default();
            let message = match diagnostic.kind {
                DiagnosticKind::Syntax | DiagnosticKind::Io => diagnostic.message.clone(),
                _ => diagnostic.to_string(),
            };
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .w(scaled(64., cx))
                        .flex_none()
                        .text_color(colors.muted_foreground)
                        .child(line),
                )
                .child(div().flex_1().min_w_0().text_color(color).child(message))
        });
        Some(
            v_flex()
                .id("settings-diagnostics")
                .debug_selector(|| "settings-diagnostics".to_owned())
                .flex_none()
                .max_h(scaled(160., cx))
                .overflow_y_scroll()
                .gap_1()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(colors.border)
                .text_xs()
                .when_some(self.error.clone(), |this, error| {
                    this.child(div().text_color(colors.danger).child(error))
                })
                .children(rows)
                .when(file_ignored, |this| {
                    this.child(
                        div().text_color(colors.muted_foreground).child(
                            "Maze keeps using your last valid settings until this is fixed.",
                        ),
                    )
                })
                .into_any_element(),
        )
    }
}

impl Focusable for SettingsEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for SettingsEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let banner = self.render_banner(cx);
        let diagnostics = self.render_diagnostics(cx);
        v_flex()
            .size_full()
            .key_context("SettingsEditor")
            .bg(colors.background)
            .text_color(colors.foreground)
            .on_action(cx.listener(|this, _: &SaveSettings, _, cx| {
                this.save(cx);
            }))
            .children(banner)
            .child(
                div().flex_1().min_h_0().child(
                    Editor::new(&self.editor)
                        .h(relative(1.))
                        .bordered(false)
                        // The element sets the state's mode on every frame.
                        .readonly(self.is_defaults()),
                ),
            )
            .children(diagnostics)
    }
}

impl Item for SettingsEditor {
    fn tab_title(&self, _: &App) -> SharedString {
        if self.is_defaults() {
            "Default Settings".into()
        } else {
            "Settings".into()
        }
    }

    fn tab_icon(&self, _: &App) -> Option<IconName> {
        Some(IconName::Settings)
    }

    fn tab_tooltip(&self, _: &App) -> Option<SharedString> {
        Some(match &self.source {
            Source::File(file) => file.path().display().to_string().into(),
            Source::Defaults => "Maze's default settings (read-only)".into(),
        })
    }

    fn is_dirty(&self, _: &App) -> bool {
        self.dirty
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        SettingsEditor::request_close(self, window, cx)
    }

    fn serialize(&self, _: &App) -> Option<SerializedItem> {
        let data = serde_json::to_value(SerializedSettings {
            defaults: self.is_defaults(),
        })
        .ok()?;
        Some(SerializedItem::new(SETTINGS_KIND, data))
    }

    fn clone_on_split(
        &self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Option<Entity<Self>> {
        // The user's file is edited in one place only; the defaults can be shown twice.
        self.is_defaults()
            .then(|| cx.new(|cx| Self::defaults(window, cx)))
    }
}

//! The compose window: header form (From, To, Cc, Bcc, Subject), a Markdown body,
//! attachments and a footer with the status line and the Send button.
//!
//! # Lifecycle
//!
//! - **Changes** to any field mark the window dirty and (re)start the autosave timer
//!   ([`ComposerOptions::autosave_after`]). A blank draft that was never saved isn't
//!   saved; a draft whose addresses don't parse waits until they do.
//! - **Send** (`cmd-enter`, also from inside the body, where the text field would
//!   otherwise insert a line break) validates the fields, hands the draft to
//!   [`ComposeBackend::send`] and, once it is accepted (sent or queued), emits
//!   [`ComposerEvent::Sent`] and closes the window. Errors keep the window open.
//! - **Close** (Escape, the window's close button, or closing its tab) closes at once
//!   when nothing changed since the last save; otherwise it asks whether to save the
//!   draft. "Don't Save" deletes the draft this composer autosaved, but keeps a draft
//!   that existed before it opened.
//!
//! # Footer and "Sent with Maze"
//!
//! The configured footer ([`ComposeBackend::footer`], the account's `compose.footer`)
//! and, while "Add “Sent with Maze”" is checked, the Maze line are shown muted under
//! the body: they are added when the message is sent, not part of the text you edit.
//! The checkbox starts from the draft ([`Draft::support_maze`], new drafts take the
//! `compose.support_maze` setting) and is saved with it.
//!
//! # In a tab
//!
//! Maze opens drafts in tabs of the main window ([`ComposerOptions::in_tab`], ADR-018):
//! the composer is then a [`ui::panes::Item`] titled with the draft's random two-word
//! name (the subject is the tab's tooltip), with a dot while there are unsaved changes.
//! Closing (or sending) closes the tab instead of a window. "Open in New Window"
//! (`composer::OpenInNewWindow`) hands the draft to a window of its own.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyWindowHandle, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions,
    PromptLevel, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Task, Window, WindowBounds, WindowOptions, div, prelude::FluentBuilder as _, px, relative,
    size,
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IndexPath, Sizable as _, TitleBar,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{self, Input, InputEvent, InputState, Textarea, TextareaState},
    select::{Select, SelectEvent, SelectItem, SelectState},
    v_flex,
};
use mazemail_types::compose::{Draft, DraftAttachment, SENT_WITH_MAZE};
use mazemail_types::{Address, DraftId, IdentityId};

use ui::assets::IconName;
use ui::names::NameRng;
use ui::panes::{Item, ItemEvent, SerializedItem};

use crate::actions::{Attach, Close, OpenInNewWindow, SaveDraft, Send, ToggleCcBcc};
use crate::backend::{ComposeBackend, IdentityChoice, SendOutcome};

/// Default window size.
const DEFAULT_SIZE: (f32, f32) = (760., 640.);
/// The window can't get smaller than this.
const MIN_SIZE: (f32, f32) = (480., 360.);
/// The window title without a subject.
const UNTITLED: &str = "New Message";

/// The layout kind of draft tabs (`{"kind": "compose", "data": {"draft": 17}}`).
pub const COMPOSE_KIND: &str = "compose";

/// What a draft tab keeps in the layout file (its name is the tab's stored title).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SerializedDraft {
    /// The saved draft.
    pub draft: DraftId,
}

/// How a compose window starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerOptions {
    /// The draft to edit (prefilled for replies and forwards).
    pub draft: Draft,
    /// Its id, if it is saved already.
    pub draft_id: Option<DraftId>,
    /// How long after the last change the draft is autosaved.
    pub autosave_after: Duration,
    /// The composer is a tab of the main window rather than a window of its own.
    pub in_tab: bool,
}

impl ComposerOptions {
    /// Options for writing `draft` (not saved yet) in a window of its own.
    pub fn new(draft: Draft) -> Self {
        Self {
            draft,
            draft_id: None,
            autosave_after: Duration::from_secs(2),
            in_tab: false,
        }
    }

    /// Options for writing `draft` in a tab of the main window.
    pub fn in_tab(draft: Draft) -> Self {
        Self {
            in_tab: true,
            ..Self::new(draft)
        }
    }
}

/// What a compose window reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerEvent {
    /// The message was handed to the outbox (and the window or tab closes).
    Sent(SendOutcome),
    /// "Open in New Window" from a tab: the tab closed; open a window with these
    /// options.
    OpenInNewWindow(Box<ComposerOptions>),
}

/// The compose window couldn't be opened.
#[derive(Debug, thiserror::Error)]
#[error("could not open the compose window: {0}")]
pub struct OpenComposerError(pub String);

/// An identity in the From picker.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IdentityItem {
    id: IdentityId,
    title: SharedString,
}

impl IdentityItem {
    fn of(choice: &IdentityChoice, several_accounts: bool) -> Self {
        let from = choice.from_line();
        let title = if several_accounts && !choice.account.is_empty() {
            format!("{from} · {}", choice.account)
        } else {
            from
        };
        Self {
            id: choice.id,
            title: title.into(),
        }
    }
}

impl SelectItem for IdentityItem {
    type Value = IdentityId;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

type IdentitySelect = SelectState<Vec<IdentityItem>>;

/// What the status line says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    None,
    Saving,
    Saved,
    Sending,
    Error(SharedString),
}

/// The compose window's root view.
pub struct Composer {
    backend: Arc<dyn ComposeBackend>,
    focus: FocusHandle,
    /// The draft the window opened with: reply fields and origin come from it.
    base: Draft,
    draft_id: Option<DraftId>,
    /// The draft was saved before the window opened ("Don't Save" keeps it).
    opened_saved: bool,
    identities: Vec<IdentityChoice>,
    identity: Option<IdentityId>,
    from: Option<Entity<IdentitySelect>>,
    to: Entity<InputState>,
    cc: Entity<InputState>,
    bcc: Entity<InputState>,
    subject: Entity<InputState>,
    body: Entity<TextareaState>,
    attachments: Vec<DraftAttachment>,
    /// "Add “Sent with Maze”" is checked.
    support_maze: bool,
    /// The configured footer of the selected identity's account (a preview).
    footer: SharedString,
    cc_visible: bool,
    dirty: bool,
    sending: bool,
    closed: bool,
    status: Status,
    autosave_after: Duration,
    autosave: Option<Task<()>>,
    title: SharedString,
    /// The draft's two-word name, the tab's title.
    name: SharedString,
    in_tab: bool,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for Composer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Composer")
            .field("draft_id", &self.draft_id)
            .field("identity", &self.identity)
            .field("dirty", &self.dirty)
            .field("sending", &self.sending)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<ComposerEvent> for Composer {}
impl EventEmitter<ItemEvent> for Composer {}

fn title_for(subject: &str) -> SharedString {
    let subject = subject.trim();
    if subject.is_empty() {
        UNTITLED.into()
    } else {
        subject.to_owned().into()
    }
}

/// "12 KB", "1.2 MB".
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    match bytes {
        0..KB => format!("{bytes} B"),
        KB..MB => format!("{} KB", bytes.div_ceil(KB)),
        _ => format!("{:.1} MB", bytes as f64 / MB as f64),
    }
}

/// Parses one address field; the error names the entry that isn't an address.
fn parse_field(text: &str) -> Result<Vec<Address>, String> {
    Address::parse_list(text).map_err(|err| format!("“{}” isn't an email address.", err.entry))
}

impl Composer {
    /// Builds the window content.
    pub fn new(
        backend: Arc<dyn ComposeBackend>,
        options: ComposerOptions,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let draft = options.draft;
        let identities = backend.identities();
        let identity = draft
            .identity
            .filter(|id| identities.iter().any(|i| i.id == *id))
            .or_else(|| identities.first().map(|i| i.id));
        let footer = backend.footer(identity).trim().to_owned().into();

        let line = |placeholder: &'static str,
                    value: String,
                    window: &mut Window,
                    cx: &mut Context<'_, Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let to = line("To", Address::format_list(&draft.to), window, cx);
        let cc = line("Cc", Address::format_list(&draft.cc), window, cx);
        let bcc = line("Bcc", Address::format_list(&draft.bcc), window, cx);
        let subject = line("Subject", draft.subject.clone(), window, cx);
        let body_text = draft.body_markdown.clone();
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .placeholder("Write in Markdown…")
                .default_value(body_text)
        });

        let several_accounts = identities
            .iter()
            .any(|i| identities.first().is_some_and(|f| f.account != i.account));
        let from = (!identities.is_empty()).then(|| {
            let items: Vec<IdentityItem> = identities
                .iter()
                .map(|i| IdentityItem::of(i, several_accounts))
                .collect();
            let selected = identity
                .and_then(|id| items.iter().position(|i| i.id == id))
                .map(IndexPath::new);
            cx.new(|cx| SelectState::new(items, selected, window, cx))
        });

        let mut subscriptions = vec![
            cx.subscribe_in(&to, window, Self::on_input_event),
            cx.subscribe_in(&cc, window, Self::on_input_event),
            cx.subscribe_in(&bcc, window, Self::on_input_event),
            cx.subscribe_in(&subject, window, Self::on_subject_event),
            cx.subscribe_in(&body, window, Self::on_body_event),
        ];
        if let Some(from) = &from {
            subscriptions.push(cx.subscribe_in(from, window, Self::on_identity_event));
        }

        let focus = cx.focus_handle();
        let title = title_for(&draft.subject);
        if options.in_tab {
            // Activating the tab focuses the composer: on into a field.
            subscriptions.push(cx.on_focus(&focus, window, |this, window, cx| {
                this.focus_first_field(window, cx);
            }));
        } else {
            // The window's close button runs the same flow as Escape.
            let this = cx.entity().downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                this.update(cx, |this, cx| this.request_close(window, cx))
                    .unwrap_or(true)
            });
            window.set_window_title(&title);
        }
        let name: SharedString = if draft.name.trim().is_empty() {
            ui::names::two_word_name::<&str>(&[], &mut NameRng::from_entropy()).into()
        } else {
            draft.name.clone().into()
        };
        let cc_visible = !draft.cc.is_empty() || !draft.bcc.is_empty();
        let initial_focus = if draft.to.is_empty() {
            to.focus_handle(cx)
        } else {
            body.focus_handle(cx)
        };
        window.focus(&initial_focus, cx);

        Self {
            backend,
            focus,
            attachments: draft.attachments.clone(),
            support_maze: draft.support_maze,
            footer,
            base: draft,
            draft_id: options.draft_id,
            opened_saved: options.draft_id.is_some(),
            identities,
            identity,
            from,
            to,
            cc,
            bcc,
            subject,
            body,
            cc_visible,
            dirty: false,
            sending: false,
            closed: false,
            status: Status::None,
            autosave_after: options.autosave_after,
            autosave: None,
            title,
            name,
            in_tab: options.in_tab,
            _subscriptions: subscriptions,
        }
    }

    fn focus_first_field(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let to_empty = self.to.read(cx).value().trim().is_empty();
        let field = if to_empty {
            self.to.focus_handle(cx)
        } else {
            self.body.focus_handle(cx)
        };
        window.focus(&field, cx);
    }

    /// The draft's two-word name.
    pub fn name(&self) -> &SharedString {
        &self.name
    }

    // ---- state accessors ------------------------------------------------------------

    /// The error the status line shows, if any.
    pub fn error(&self) -> Option<SharedString> {
        match &self.status {
            Status::Error(message) => Some(message.clone()),
            _ => None,
        }
    }

    /// Whether the Cc and Bcc fields are shown.
    pub fn cc_visible(&self) -> bool {
        self.cc_visible
    }

    /// Whether something changed since the last save.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the window has closed itself (sent, or closed by the user).
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The saved draft's id, once there is one.
    pub fn draft_id(&self) -> Option<DraftId> {
        self.draft_id
    }

    /// The selected identity.
    pub fn identity(&self) -> Option<IdentityId> {
        self.identity
    }

    /// Whether "Add “Sent with Maze”" is checked.
    pub fn support_maze(&self) -> bool {
        self.support_maze
    }

    /// What is added below the message when it is sent, as the preview under the body
    /// shows it: the footer and the Maze line, each when there is one.
    pub fn footer_preview(&self) -> Vec<SharedString> {
        let mut lines = Vec::new();
        if !self.footer.is_empty() {
            lines.push(self.footer.clone());
        }
        if self.support_maze {
            lines.push(SENT_WITH_MAZE.into());
        }
        lines
    }

    /// Checks or unchecks "Add “Sent with Maze”" (saved with the draft).
    pub fn set_support_maze(&mut self, on: bool, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.support_maze != on {
            self.support_maze = on;
            self.changed(window, cx);
        }
    }

    /// The draft as the fields describe it, or the first problem with the addresses.
    pub fn draft(&self, cx: &App) -> Result<Draft, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        Ok(Draft {
            identity: self.identity,
            to: parse_field(&value(&self.to))?,
            cc: parse_field(&value(&self.cc))?,
            bcc: parse_field(&value(&self.bcc))?,
            subject: value(&self.subject).trim().to_owned(),
            body_markdown: self.body.read(cx).value().to_string(),
            in_reply_to: self.base.in_reply_to.clone(),
            references: self.base.references.clone(),
            attachments: self.attachments.clone(),
            origin: self.base.origin,
            name: self.name.to_string(),
            support_maze: self.support_maze,
        })
    }

    // ---- changes and autosave -------------------------------------------------------

    fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            self.changed(window, cx);
        }
    }

    fn on_subject_event(
        &mut self,
        subject: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            let title = title_for(&subject.read(cx).value());
            if title != self.title {
                if !self.in_tab {
                    window.set_window_title(&title);
                }
                self.title = title;
                cx.emit(ItemEvent::UpdateTab);
            }
            self.changed(window, cx);
        }
    }

    fn on_body_event(
        &mut self,
        _: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            self.changed(window, cx);
        }
    }

    fn on_identity_event(
        &mut self,
        _: &Entity<IdentitySelect>,
        event: &SelectEvent<Vec<IdentityItem>>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let SelectEvent::Confirm(Some(id)) = event else {
            return;
        };
        if self.identity != Some(*id) {
            self.identity = Some(*id);
            // Another account may have another footer.
            self.footer = self.backend.footer(self.identity).trim().to_owned().into();
            self.changed(window, cx);
        }
    }

    /// Something changed: mark dirty and restart the autosave timer.
    fn changed(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.closed {
            return;
        }
        if !self.dirty {
            cx.emit(ItemEvent::UpdateTab);
        }
        self.dirty = true;
        if matches!(self.status, Status::Saved | Status::Error(_)) && !self.sending {
            self.status = Status::None;
        }
        let timer = cx.background_executor().timer(self.autosave_after);
        self.autosave = Some(cx.spawn_in(window, async move |this, cx| {
            timer.await;
            let _ = this.update_in(cx, |this, window, cx| this.autosave(window, cx));
        }));
        cx.notify();
    }

    /// The autosave timer fired.
    fn autosave(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.autosave = None;
        if !self.dirty || self.sending || self.closed {
            return;
        }
        // Addresses that don't parse yet: wait for the next change.
        let Ok(draft) = self.draft(cx) else {
            return;
        };
        if draft.is_blank() && self.draft_id.is_none() {
            return;
        }
        self.save(draft, false, window, cx);
    }

    /// Saves `draft`; closes the window afterwards if `then_close`.
    fn save(
        &mut self,
        draft: Draft,
        then_close: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.autosave = None;
        self.status = Status::Saving;
        let saving = cx.background_spawn(self.backend.save_draft(self.draft_id, draft));
        cx.spawn_in(window, async move |this, cx| {
            let result = saving.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    // An autosave that finished after the window was sent or discarded
                    // mustn't leave a draft behind.
                    Ok(id) if this.closed && !then_close => {
                        if !this.opened_saved {
                            let deleting = cx.background_spawn(this.backend.delete_draft(id));
                            cx.background_spawn(async move {
                                if let Err(err) = deleting.await {
                                    tracing::warn!(%err, "removing a late autosave failed");
                                }
                            })
                            .detach();
                        }
                    }
                    Ok(id) => {
                        if this.draft_id != Some(id) {
                            this.draft_id = Some(id);
                            // The tab can be restored now.
                            cx.emit(ItemEvent::Edited);
                        }
                        // Changes typed while saving keep the window dirty (their own
                        // timer saves them).
                        if this.autosave.is_none() {
                            this.dirty = false;
                        }
                        cx.emit(ItemEvent::UpdateTab);
                        this.status = Status::Saved;
                        if then_close {
                            this.close_window(window, cx);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(%err, "saving the draft failed");
                        this.status =
                            Status::Error(format!("The draft couldn't be saved: {err}").into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    // ---- actions ----------------------------------------------------------------------

    fn save_now(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        match self.draft(cx) {
            Ok(draft) => self.save(draft, false, window, cx),
            Err(message) => {
                self.status = Status::Error(message.into());
                cx.notify();
            }
        }
    }

    /// Validates the fields and sends.
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.sending || self.closed {
            return;
        }
        let draft = match self.draft(cx) {
            Ok(draft) => draft,
            Err(message) => {
                self.status = Status::Error(message.into());
                cx.notify();
                return;
            }
        };
        if self.identities.is_empty() || draft.identity.is_none() {
            self.status = Status::Error("Add an account to send mail.".into());
            cx.notify();
            return;
        }
        if draft.recipients().next().is_none() {
            self.status = Status::Error("Add at least one recipient.".into());
            cx.notify();
            return;
        }
        self.sending = true;
        self.autosave = None;
        self.status = Status::Sending;
        let sending = cx.background_spawn(self.backend.send(self.draft_id, draft));
        cx.spawn_in(window, async move |this, cx| {
            let result = sending.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = false;
                match result {
                    Ok(outcome) => {
                        this.dirty = false;
                        cx.emit(ComposerEvent::Sent(outcome));
                        this.close_window(window, cx);
                    }
                    Err(err) => {
                        this.status = Status::Error(err.message.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn attach(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let paths: Vec<PathBuf> = match picked.await {
                Ok(Ok(Some(paths))) => paths,
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(err)) => {
                    tracing::warn!(%err, "the file picker failed");
                    return;
                }
            };
            let files: Vec<DraftAttachment> = paths
                .into_iter()
                .filter_map(|path| {
                    let meta = std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
                    let name = path.file_name()?.to_string_lossy().into_owned();
                    Some(DraftAttachment {
                        path,
                        name,
                        size: meta.len(),
                    })
                })
                .collect();
            let _ = this.update_in(cx, |this, window, cx| {
                this.add_attachments(files, window, cx)
            });
        })
        .detach();
    }

    /// Adds files to the message (skipping ones attached already).
    pub fn add_attachments(
        &mut self,
        files: Vec<DraftAttachment>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let before = self.attachments.len();
        for file in files {
            if !self.attachments.iter().any(|a| a.path == file.path) {
                self.attachments.push(file);
            }
        }
        if self.attachments.len() != before {
            self.changed(window, cx);
        }
    }

    fn remove_attachment(&mut self, ix: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        if ix < self.attachments.len() {
            self.attachments.remove(ix);
            self.changed(window, cx);
        }
    }

    fn toggle_cc(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.cc_visible = !self.cc_visible;
        if self.cc_visible {
            let focus = self.cc.focus_handle(cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// Escape or the close button. Returns whether the window may close right away;
    /// otherwise it asks first and closes itself after the answer.
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        if self.closed {
            return true;
        }
        if self.sending {
            return false;
        }
        let blank = self.draft(cx).is_ok_and(|d| d.is_blank());
        if !self.dirty || (blank && self.draft_id.is_none()) {
            self.autosave = None;
            self.closed = true;
            return true;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            "Save this message as a draft?",
            Some("Your changes will be lost if you don't save them."),
            &["Save", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| match answer {
                0 => match this.draft(cx) {
                    Ok(draft) => this.save(draft, true, window, cx),
                    Err(message) => {
                        this.status = Status::Error(message.into());
                        cx.notify();
                    }
                },
                1 => {
                    // A draft this window autosaved goes; one that existed before
                    // stays as it was last saved.
                    if let Some(id) = this.draft_id.filter(|_| !this.opened_saved) {
                        let deleting = cx.background_spawn(this.backend.delete_draft(id));
                        cx.background_spawn(async move {
                            if let Err(err) = deleting.await {
                                tracing::warn!(%err, "discarding the draft failed");
                            }
                        })
                        .detach();
                    }
                    this.close_window(window, cx);
                }
                _ => {}
            });
        })
        .detach();
        false
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.request_close(window, cx) {
            self.close_window(window, cx);
        }
    }

    /// Closes the window, or the tab.
    fn close_window(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.autosave = None;
        self.closed = true;
        if self.in_tab {
            cx.emit(ItemEvent::Close);
        } else {
            window.remove_window();
        }
    }

    /// "Open in New Window": the tab closes and the app opens a window with the draft
    /// as it is now.
    fn open_in_new_window(&mut self, cx: &mut Context<'_, Self>) {
        if !self.in_tab || self.sending || self.closed {
            return;
        }
        let draft = match self.draft(cx) {
            Ok(draft) => draft,
            Err(message) => {
                self.status = Status::Error(message.into());
                cx.notify();
                return;
            }
        };
        let options = ComposerOptions {
            draft,
            draft_id: self.draft_id,
            autosave_after: self.autosave_after,
            in_tab: false,
        };
        self.autosave = None;
        self.closed = true;
        cx.emit(ComposerEvent::OpenInNewWindow(Box::new(options)));
        cx.emit(ItemEvent::Close);
    }

    // ---- rendering --------------------------------------------------------------------

    fn field_row(label: &'static str, field: impl IntoElement) -> impl IntoElement {
        h_flex()
            .gap_2()
            .items_center()
            .child(div().w(px(56.)).flex_none().text_sm().child(label))
            .child(div().flex_1().min_w_0().child(field))
    }

    fn render_attachments(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        h_flex()
            .flex_wrap()
            .gap_1()
            .children(self.attachments.iter().enumerate().map(|(ix, file)| {
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(colors.border)
                    .text_xs()
                    .child(file.name.clone())
                    .child(
                        div()
                            .text_color(colors.muted_foreground)
                            .child(format_size(file.size)),
                    )
                    .child(
                        Button::new(("remove-attachment", ix))
                            .ghost()
                            .xsmall()
                            .label("×")
                            .tooltip("Remove")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.remove_attachment(ix, window, cx);
                            })),
                    )
            }))
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let colors = cx.theme().colors;
        let no_identity = self.identities.is_empty();
        let from: gpui::AnyElement = match &self.from {
            Some(from) => Select::new(from).small().into_any_element(),
            None => div()
                .text_sm()
                .text_color(colors.muted_foreground)
                .child("Add an account to send mail")
                .into_any_element(),
        };
        let status: Option<(SharedString, gpui::Hsla)> = match &self.status {
            Status::None => None,
            Status::Saving => Some(("Saving…".into(), colors.muted_foreground)),
            Status::Saved => Some(("Draft saved".into(), colors.muted_foreground)),
            Status::Sending => Some(("Sending…".into(), colors.muted_foreground)),
            Status::Error(message) => Some((message.clone(), colors.danger)),
        };
        let footer_preview = self.footer_preview();

        v_flex()
            .id("composer")
            .size_full()
            .key_context("Composer")
            .track_focus(&self.focus)
            .bg(colors.background)
            .text_color(colors.foreground)
            // cmd-enter inside a text field: the field would insert a line break (or
            // stop the key), so catch it on the way down.
            .capture_action(cx.listener(|this, action: &input::Enter, window, cx| {
                if action.secondary {
                    cx.stop_propagation();
                    this.send(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Send, window, cx| this.send(window, cx)))
            .on_action(cx.listener(|this, _: &Close, window, cx| this.close(window, cx)))
            .on_action(cx.listener(|this, _: &SaveDraft, window, cx| this.save_now(window, cx)))
            .on_action(cx.listener(|this, _: &Attach, window, cx| this.attach(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleCcBcc, window, cx| this.toggle_cc(window, cx)))
            .on_action(cx.listener(|this, _: &OpenInNewWindow, _, cx| this.open_in_new_window(cx)))
            .when(!self.in_tab, |this| {
                this.child(
                    TitleBar::new()
                        .bg(colors.title_bar)
                        .border_b_1()
                        .border_color(colors.border)
                        .child(
                            h_flex()
                                .w_full()
                                .justify_center()
                                .text_sm()
                                .child(self.title.clone()),
                        ),
                )
            })
            .child(
                v_flex()
                    .gap_1p5()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(Self::field_row("From", from))
                    .child(Self::field_row(
                        "To",
                        h_flex()
                            .gap_1()
                            .child(div().flex_1().child(Input::new(&self.to).small()))
                            .child(
                                Button::new("toggle-cc")
                                    .ghost()
                                    .xsmall()
                                    .label("Cc Bcc")
                                    .tooltip("Show Cc and Bcc (⌘⇧C)")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.toggle_cc(window, cx);
                                    })),
                            ),
                    ))
                    .when(self.cc_visible, |this| {
                        this.child(Self::field_row("Cc", Input::new(&self.cc).small()))
                            .child(Self::field_row("Bcc", Input::new(&self.bcc).small()))
                    })
                    .child(Self::field_row(
                        "Subject",
                        Input::new(&self.subject).small(),
                    ))
                    .when(!self.attachments.is_empty(), |this| {
                        this.child(self.render_attachments(cx))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_3()
                    .py_2()
                    .child(Textarea::new(&self.body).h(relative(1.)).appearance(false)),
            )
            .when(!footer_preview.is_empty(), |this| {
                this.child(
                    v_flex()
                        .id("footer-preview")
                        .debug_selector(|| "footer-preview".to_owned())
                        .flex_none()
                        .max_h(px(120.))
                        .overflow_y_scroll()
                        .gap_1()
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(colors.muted_foreground)
                        .children(
                            footer_preview
                                .into_iter()
                                .map(|line| div().whitespace_normal().child(line)),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(colors.border)
                    .child(
                        Button::new("attach")
                            .small()
                            .label("Attach…")
                            .tooltip("Attach files (⌘⇧A)")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.attach(window, cx);
                            })),
                    )
                    .child(
                        Checkbox::new("support-maze")
                            .debug_selector(|| "support-maze".to_owned())
                            .small()
                            .label("Add “Sent with Maze”")
                            .tooltip("Support Maze: adds a short line with a link to mazemail.dev")
                            .checked(self.support_maze)
                            .on_click({
                                let this = cx.entity().downgrade();
                                move |checked: &bool, window, cx| {
                                    let checked = *checked;
                                    this.update(cx, |this, cx| {
                                        this.set_support_maze(checked, window, cx);
                                    })
                                    .ok();
                                }
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .when_some(status, |this, (text, color)| {
                                this.text_color(color).child(text)
                            }),
                    )
                    .child(
                        Button::new("send")
                            .primary()
                            .small()
                            .label(if self.sending { "Sending…" } else { "Send" })
                            .tooltip("Send (⌘↩)")
                            .disabled(self.sending || no_identity)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.send(window, cx);
                            })),
                    ),
            )
    }
}

impl Item for Composer {
    fn tab_title(&self, _: &App) -> SharedString {
        self.name.clone()
    }

    fn tab_icon(&self, _: &App) -> Option<IconName> {
        Some(IconName::SquarePen)
    }

    fn tab_tooltip(&self, _: &App) -> Option<SharedString> {
        Some(self.title.clone())
    }

    fn is_dirty(&self, _: &App) -> bool {
        self.dirty
    }

    fn request_close(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        Composer::request_close(self, window, cx)
    }

    fn serialize(&self, _: &App) -> Option<SerializedItem> {
        let data = serde_json::to_value(SerializedDraft {
            draft: self.draft_id?,
        })
        .ok()?;
        Some(SerializedItem::new(COMPOSE_KIND, data).with_title(self.name.to_string()))
    }
}

/// Opens a compose window. Returns its handle and root view (subscribe to
/// [`ComposerEvent`]s on it).
pub fn open_composer(
    backend: Arc<dyn ComposeBackend>,
    options: ComposerOptions,
    cx: &mut App,
) -> Result<(AnyWindowHandle, Entity<Composer>), OpenComposerError> {
    let title = title_for(&options.draft.subject);
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(px(DEFAULT_SIZE.0), px(DEFAULT_SIZE.1)),
            cx,
        )),
        window_min_size: Some(size(px(MIN_SIZE.0), px(MIN_SIZE.1))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(title),
            ..TitleBar::title_bar_options()
        }),
        app_id: Some(mazemail_types::TECHNICAL_NAME.to_owned()),
        ..TitleBar::window_options()
    };
    gpui_kit::open_window(window_options, cx, move |window, cx| {
        cx.new(|cx| Composer::new(backend, options, window, cx))
    })
    .map_err(|error| OpenComposerError(error.to_string()))
}

#[cfg(test)]
mod tests;

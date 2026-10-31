//! The To, Cc and Bcc fields: addresses as chips in front of a text field, completion
//! from the address books and the addresses you wrote to, and chips that can be
//! dragged between the fields (PLAN-contacts §1).
//!
//! # Typing
//!
//! - A comma or semicolon after an address, Enter, Tab or leaving the field turns the
//!   typed text into chips. Pasting a list (`Anna <anna@example.com>, bob@example.org`)
//!   makes one chip per address. The text is split like [`Address::parse_list`] does:
//!   separators inside quotes or angle brackets don't count.
//! - Text that isn't an address becomes a red chip; sending (and autosaving) waits
//!   until it is fixed, with the same message the plain field gave
//!   (`“nobody” isn't an email address.`). Double-click a chip to edit it as text.
//! - Text not turned into a chip yet still counts: [`RecipientField::addresses`] parses
//!   it as before, so `cmd-enter` right after typing an address sends to it.
//!
//! # Completion
//!
//! A moment ([`SUGGEST_DEBOUNCE`]) after the typing stops, the field asks the
//! [`CompleteAddress`] function off the UI thread and shows the suggestions under the
//! field. Up and Down move, Enter or Tab take one, Escape closes the list; a click
//! takes one too. Addresses in the field already aren't offered.
//!
//! # Chips
//!
//! Click a chip to select it; Backspace or Delete removes the selected chip. Backspace
//! in the empty text field selects the last chip (a second Backspace removes it), Left
//! and Right walk through the chips. Chips can be dragged to another recipient field
//! (or elsewhere in the same one); see [`DraggedRecipient`].
//!
//! Only the text field takes the keyboard focus, so Tab moves from field to field as
//! before.
//!
//! The component lives in the composer rather than `ui` because it is about mail
//! addresses ([`Address`], from `mazemail_types`, which `ui` doesn't know).

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels,
    Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task,
    WeakEntity, Window, anchored, deferred, div, prelude::FluentBuilder as _, px, relative,
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, ThemeStyled as _, h_flex,
    input::{self, Input, InputEvent, InputState},
    tooltip::Tooltip,
    v_flex,
};
use mazemail_types::Address;
use ui::components::{Avatar, AvatarTone, initials};
use ui::theme::scaled;

/// Finds addresses for what was typed (the backend's address completion). Called on a
/// background thread.
pub type CompleteAddress = Arc<dyn Fn(&str) -> Vec<Address> + Send + Sync>;

/// How long after the last key the suggestions are looked up.
pub const SUGGEST_DEBOUNCE: Duration = Duration::from_millis(120);

/// At most this many suggestions are shown.
const MAX_SUGGESTIONS: usize = 8;

/// A field's height without chips wrapping onto more lines, as gpui-kit's small text
/// fields (`h_6`), in rems.
const FIELD_HEIGHT: f32 = 1.5;

/// The height of a chip: the field's less its border and padding (1 px each).
fn chip_height(window: &Window) -> Pixels {
    window.rem_size() * FIELD_HEIGHT - px(4.)
}

/// One entry of a recipient field.
#[derive(Clone, PartialEq, Eq)]
pub enum Recipient {
    /// An address.
    Address(Address),
    /// Text that isn't an address (as typed, trimmed).
    Invalid(String),
}

impl std::fmt::Debug for Recipient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Addresses are personal data: never in logs.
        match self {
            Self::Address(_) => f.write_str("Recipient::Address(..)"),
            Self::Invalid(_) => f.write_str("Recipient::Invalid(..)"),
        }
    }
}

impl Recipient {
    /// What one entry of a typed list means; `None` for blank text.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        // `parse_list` of a single entry: the same rules as the plain field had.
        Some(match Address::parse_list(text) {
            Ok(mut list) if list.len() == 1 => Self::Address(list.remove(0)),
            _ => Self::Invalid(text.to_owned()),
        })
    }

    /// The address, unless the entry isn't one.
    pub fn address(&self) -> Option<&Address> {
        match self {
            Self::Address(address) => Some(address),
            Self::Invalid(_) => None,
        }
    }

    /// What the chip says: the name, else the address (or the text that isn't one).
    pub fn label(&self) -> SharedString {
        match self {
            Self::Address(address) => address
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| address.email.clone())
                .into(),
            Self::Invalid(text) => text.clone().into(),
        }
    }

    /// The chip's tooltip: the full address, or why it is red.
    pub fn detail(&self) -> SharedString {
        match self {
            Self::Address(address) => address.to_string().into(),
            Self::Invalid(text) => format!("“{text}” isn't an email address").into(),
        }
    }

    /// The text to edit when the chip is double-clicked.
    fn edit_text(&self) -> String {
        match self {
            Self::Address(address) => address.to_string(),
            Self::Invalid(text) => text.clone(),
        }
    }

    fn same_mailbox(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Address(a), Self::Address(b)) => a.same_mailbox(b),
            (Self::Invalid(a), Self::Invalid(b)) => a == b,
            _ => false,
        }
    }
}

/// Splits typed text at the separators of an address list (`,` `;` and line breaks
/// outside quotes and angle brackets, as [`Address::parse_list`] does): the finished
/// entries (each followed by a separator) and the rest.
fn split_typed(text: &str) -> (Vec<&str>, &str) {
    let mut entries = Vec::new();
    let mut start = 0;
    let (mut quoted, mut angle, mut escaped) = (false, false, false);
    for (ix, c) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' | ';' | '\n' | '\r' if !quoted && !angle => {
                entries.push(&text[start..ix]);
                start = ix + c.len_utf8();
            }
            _ => {}
        }
    }
    (entries, &text[start..])
}

/// Initials for the avatar: "Anna Example" → "AE", "bob@example.org" → "B", and "!"
/// for text that isn't an address.
fn recipient_initials(recipient: &Recipient) -> SharedString {
    match recipient {
        Recipient::Address(address) => initials(address.name.as_deref(), &address.email),
        Recipient::Invalid(_) => "!".into(),
    }
}

/// The round picture in front of a recipient: [`ui::components::Avatar`], the one
/// look for people across Maze (initials until contact photos arrive).
fn avatar(recipient: &Recipient, size: Pixels, on_accent: bool, _: &App) -> AnyElement {
    let tone = match recipient {
        Recipient::Address(_) => AvatarTone::Accent,
        Recipient::Invalid(_) => AvatarTone::Danger,
    };
    Avatar::new(recipient_initials(recipient), size)
        .tone(if on_accent {
            AvatarTone::OnAccent
        } else {
            tone
        })
        .into_any_element()
}

/// A chip as drawn in a field and as the drag preview.
fn chip_body(
    id: impl Into<gpui::ElementId>,
    recipient: &Recipient,
    selected: bool,
    height: Pixels,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    let invalid = matches!(recipient, Recipient::Invalid(_));
    let (bg, fg) = if selected {
        (theme.primary, theme.primary_foreground)
    } else if invalid {
        (theme.danger.opacity(0.08), theme.danger)
    } else {
        // Neutral and visible on the field's background in light and dark themes.
        (theme.foreground.opacity(0.08), theme.foreground)
    };
    h_flex()
        .id(id)
        .flex_none()
        .min_w_0()
        .max_w(scaled(280., cx))
        .h(height)
        .pl(px(2.))
        .pr(scaled(7., cx))
        .gap(scaled(5., cx))
        .rounded(theme.radius_full())
        .border_1()
        .border_color(if invalid && !selected {
            theme.danger
        } else {
            gpui::transparent_black()
        })
        .bg(bg)
        .text_color(fg)
        .text_sm()
        .child(avatar(recipient, height - px(4.), selected, cx))
        .child(div().min_w_0().truncate().child(recipient.label()))
}

/// A chip being dragged out of a [`RecipientField`]; also its drag preview. Dropping
/// it on another recipient field moves the recipient there; on a chip, in front of
/// that chip.
#[derive(Clone)]
pub struct DraggedRecipient {
    /// The field the chip comes from.
    pub source: WeakEntity<RecipientField>,
    /// Its position there when the drag started.
    pub index: usize,
    /// The recipient.
    pub recipient: Recipient,
}

impl std::fmt::Debug for DraggedRecipient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DraggedRecipient")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl Render for DraggedRecipient {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let border = cx.theme().drag_border;
        let height = chip_height(window);
        chip_body("dragged-recipient", &self.recipient, false, height, cx)
            .border_color(border)
            .shadow_md()
    }
}

/// What a recipient field reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecipientFieldEvent {
    /// The chips or the typed text changed.
    Changed,
    /// A chip was dropped on this field (it changed, too).
    Dropped,
}

/// A To, Cc or Bcc field (see the [module docs](self)).
pub struct RecipientField {
    /// Names the field's elements (`to`, `cc`, `bcc`).
    name: SharedString,
    recipients: Vec<Recipient>,
    input: Entity<InputState>,
    /// The selected chip.
    selected: Option<usize>,
    complete: Option<CompleteAddress>,
    suggestions: Vec<Address>,
    highlighted: usize,
    suggesting: Option<Task<()>>,
    /// The chip of this field being dragged (until the drag ends).
    dragging: Option<usize>,
    /// Shown in the empty field; hidden once there are chips.
    placeholder: SharedString,
    placeholder_shown: bool,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for RecipientField {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecipientField")
            .field("name", &self.name)
            .field("recipients", &self.recipients.len())
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<RecipientFieldEvent> for RecipientField {}

impl Focusable for RecipientField {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl RecipientField {
    /// A field named `name` (element ids), showing `placeholder` while empty, with
    /// `addresses` as chips. `complete` looks up suggestions (none without).
    pub fn new(
        name: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        addresses: &[Address],
        complete: Option<CompleteAddress>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let placeholder = placeholder.into();
        let placeholder_shown = addresses.is_empty();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(if placeholder_shown {
                placeholder.clone()
            } else {
                SharedString::default()
            })
        });
        let subscriptions = vec![cx.subscribe_in(&input, window, Self::on_input_event)];
        Self {
            name: name.into(),
            recipients: addresses.iter().cloned().map(Recipient::Address).collect(),
            input,
            selected: None,
            complete,
            suggestions: Vec::new(),
            highlighted: 0,
            suggesting: None,
            dragging: None,
            placeholder,
            placeholder_shown,
            _subscriptions: subscriptions,
        }
    }

    // ---- state --------------------------------------------------------------------

    /// The chips.
    pub fn recipients(&self) -> &[Recipient] {
        &self.recipients
    }

    /// The text typed after the chips.
    pub fn text(&self, cx: &App) -> SharedString {
        self.input.read(cx).value()
    }

    /// No chips and no text.
    pub fn is_empty(&self, cx: &App) -> bool {
        self.recipients.is_empty() && self.text(cx).trim().is_empty()
    }

    /// The selected chip.
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The suggestions shown under the field (empty: none shown).
    pub fn suggestions(&self) -> &[Address] {
        &self.suggestions
    }

    /// The highlighted suggestion (Enter takes it).
    pub fn highlighted(&self) -> usize {
        self.highlighted
    }

    /// Whether one of this field's chips is being dragged. Only meaningful while a
    /// drag is active ([`App::has_active_drag`]).
    pub fn is_dragging(&self) -> bool {
        self.dragging.is_some()
    }

    /// The field's addresses: the chips, then the text not turned into chips yet
    /// (parsed as a list). The error names the first entry that isn't an address,
    /// as the plain field's did.
    pub fn addresses(&self, cx: &App) -> Result<Vec<Address>, String> {
        let invalid = |entry: &str| format!("“{entry}” isn't an email address.");
        let mut out = Vec::with_capacity(self.recipients.len());
        for recipient in &self.recipients {
            match recipient {
                Recipient::Address(address) => out.push(address.clone()),
                Recipient::Invalid(text) => return Err(invalid(text)),
            }
        }
        let typed = Address::parse_list(&self.text(cx)).map_err(|err| invalid(&err.entry))?;
        out.extend(typed);
        Ok(out)
    }

    // ---- editing ------------------------------------------------------------------

    fn changed(&mut self, cx: &mut Context<'_, Self>) {
        cx.emit(RecipientFieldEvent::Changed);
        cx.notify();
    }

    /// Adds a chip at `at` (the end for `None`), unless the field has that address
    /// already. Returns where it is.
    fn insert(&mut self, at: Option<usize>, recipient: Recipient) -> usize {
        if let Some(existing) = self
            .recipients
            .iter()
            .position(|r| r.same_mailbox(&recipient))
        {
            return existing;
        }
        let at = at
            .unwrap_or(self.recipients.len())
            .min(self.recipients.len());
        self.recipients.insert(at, recipient);
        at
    }

    /// Turns every (non-blank) entry into a chip.
    fn push_entries<'a>(&mut self, entries: impl IntoIterator<Item = &'a str>) {
        for recipient in entries.into_iter().filter_map(Recipient::parse) {
            self.insert(None, recipient);
        }
    }

    fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// Turns the typed text into chips (Enter, Tab, leaving the field).
    pub fn commit_text(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let text = self.text(cx);
        if text.trim().is_empty() {
            return;
        }
        let (mut entries, rest) = split_typed(&text);
        entries.push(rest);
        self.push_entries(entries);
        self.set_text("", window, cx);
        self.close_suggestions();
        self.changed(cx);
    }

    fn on_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            InputEvent::Change => self.text_changed(window, cx),
            InputEvent::Blur => {
                self.close_suggestions();
                self.selected = None;
                self.commit_text(window, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    /// Typed text: entries ended by a separator become chips, the rest is looked up.
    fn text_changed(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.selected = None;
        let text = self.text(cx);
        let (entries, rest) = split_typed(&text);
        if !entries.is_empty() {
            let rest = rest.trim_start().to_owned();
            self.push_entries(entries);
            self.set_text(&rest, window, cx);
        }
        self.schedule_suggestions(cx);
        self.changed(cx);
    }

    /// Pasted text: a list (or a lone address into the empty field) becomes chips at
    /// once. Returns whether the paste was handled.
    fn paste(&mut self, pasted: &str, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let (entries, _) = split_typed(pasted);
        let text = self.text(cx);
        let lone_address = text.is_empty() && Address::parse(pasted).is_ok();
        if entries.is_empty() && !lone_address {
            return false;
        }
        let range = self.input.read(cx).selected_range();
        let (Some(before), Some(after)) = (text.get(..range.start), text.get(range.end..)) else {
            return false;
        };
        let combined = format!("{before}{pasted}{after}");
        let (mut entries, rest) = split_typed(&combined);
        entries.push(rest);
        self.push_entries(entries);
        self.set_text("", window, cx);
        self.close_suggestions();
        self.selected = None;
        self.changed(cx);
        true
    }

    fn select(&mut self, ix: Option<usize>, cx: &mut Context<'_, Self>) {
        self.selected = ix.filter(|ix| *ix < self.recipients.len());
        cx.notify();
    }

    /// Removes the chip at `ix`.
    pub fn remove(&mut self, ix: usize, cx: &mut Context<'_, Self>) -> Option<Recipient> {
        if ix >= self.recipients.len() {
            return None;
        }
        let removed = self.recipients.remove(ix);
        self.selected = None;
        self.changed(cx);
        Some(removed)
    }

    /// Double-click: the chip goes back into the text field to be edited.
    fn edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.commit_text(window, cx);
        let Some(recipient) = self.remove(ix, cx) else {
            return;
        };
        self.set_text(&recipient.edit_text(), window, cx);
        let focus = self.input.focus_handle(cx);
        window.focus(&focus, cx);
    }

    // ---- suggestions --------------------------------------------------------------

    fn close_suggestions(&mut self) {
        self.suggesting = None;
        self.suggestions.clear();
        self.highlighted = 0;
    }

    /// Looks up suggestions for the typed text once the typing pauses.
    fn schedule_suggestions(&mut self, cx: &mut Context<'_, Self>) {
        let prefix = self.text(cx).trim().to_owned();
        let Some(complete) = self.complete.clone().filter(|_| !prefix.is_empty()) else {
            self.close_suggestions();
            return;
        };
        let timer = cx.background_executor().timer(SUGGEST_DEBOUNCE);
        self.suggesting = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let lookup = prefix.clone();
            let found = cx
                .background_executor()
                .spawn(async move { complete(&lookup) })
                .await;
            let _ = this.update(cx, |this, cx| this.show_suggestions(&prefix, found, cx));
        }));
    }

    fn show_suggestions(&mut self, prefix: &str, found: Vec<Address>, cx: &mut Context<'_, Self>) {
        self.suggesting = None;
        // Typed on meanwhile: the next lookup is on its way.
        if self.text(cx).trim() != prefix {
            return;
        }
        let mut suggestions: Vec<Address> = Vec::new();
        for address in found {
            let known = self
                .recipients
                .iter()
                .filter_map(Recipient::address)
                .chain(&suggestions)
                .any(|a| a.same_mailbox(&address));
            if !known {
                suggestions.push(address);
            }
        }
        suggestions.truncate(MAX_SUGGESTIONS);
        self.suggestions = suggestions;
        self.highlighted = 0;
        cx.notify();
    }

    /// Takes suggestion `ix` as a chip.
    pub fn accept_suggestion(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(address) = self.suggestions.get(ix).cloned() else {
            return;
        };
        self.close_suggestions();
        self.insert(None, Recipient::Address(address));
        self.set_text("", window, cx);
        self.changed(cx);
    }

    fn move_highlight(&mut self, down: bool, cx: &mut Context<'_, Self>) {
        let count = self.suggestions.len();
        if count == 0 {
            return;
        }
        self.highlighted = if down {
            (self.highlighted + 1) % count
        } else {
            (self.highlighted + count - 1) % count
        };
        cx.notify();
    }

    // ---- drag and drop ------------------------------------------------------------

    /// Takes the dragged chip out of this field (it moves to another one). `None` if
    /// the field changed meanwhile.
    fn take(
        &mut self,
        ix: usize,
        recipient: &Recipient,
        cx: &mut Context<'_, Self>,
    ) -> Option<Recipient> {
        self.dragging = None;
        if self.recipients.get(ix) != Some(recipient) {
            return None;
        }
        self.remove(ix, cx)
    }

    /// A chip was dropped on this field (`at`: on the chip there, else at the end).
    fn drop_recipient(
        &mut self,
        drag: &DraggedRecipient,
        at: Option<usize>,
        cx: &mut Context<'_, Self>,
    ) {
        self.dragging = None;
        let own = drag.source == cx.entity().downgrade();
        let landed = if own {
            if self.recipients.get(drag.index) != Some(&drag.recipient) {
                return;
            }
            let recipient = self.recipients.remove(drag.index);
            // In front of the chip it was dropped on (which moved up if it came after).
            let at = at.map(|at| if at > drag.index { at - 1 } else { at });
            self.insert(at, recipient)
        } else {
            let Some(source) = drag.source.upgrade() else {
                return;
            };
            let Some(recipient) = source.update(cx, |source, cx| {
                source.take(drag.index, &drag.recipient, cx)
            }) else {
                return;
            };
            self.insert(at, recipient)
        };
        self.selected = Some(landed);
        cx.emit(RecipientFieldEvent::Dropped);
        self.changed(cx);
    }

    // ---- rendering ----------------------------------------------------------------

    fn render_chip(
        &self,
        ix: usize,
        recipient: &Recipient,
        height: Pixels,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement + use<> {
        let selected = self.selected == Some(ix);
        let detail = recipient.detail();
        let drag = DraggedRecipient {
            source: cx.entity().downgrade(),
            index: ix,
            recipient: recipient.clone(),
        };
        let this = cx.entity().downgrade();
        let name = self.name.clone();
        let id = (SharedString::from(format!("{}-chip", self.name)), ix);
        let dragged = self.dragging == Some(ix);
        chip_body(id, recipient, selected, height, cx)
            // The chip stays in place, faded, until it is dropped somewhere.
            .when(dragged, |this| this.opacity(0.4))
            .debug_selector(move || format!("{name}-chip-{ix}"))
            .cursor_pointer()
            .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    this.edit(ix, window, cx);
                } else {
                    this.select(Some(ix), cx);
                }
            }))
            .on_drag(drag, move |drag, _, _, cx| {
                let _ = this.update(cx, |this, _| this.dragging = Some(ix));
                cx.new(|_| drag.clone())
            })
            .drag_over::<DraggedRecipient>(|style, _, _, cx| {
                style.border_color(cx.theme().drag_border)
            })
            .on_drop(cx.listener(move |this, drag: &DraggedRecipient, _, cx| {
                this.drop_recipient(drag, Some(ix), cx);
            }))
    }

    fn render_suggestions(&self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let name = self.name.clone();
        v_flex()
            .id(SharedString::from(format!("{}-suggestions", self.name)))
            .debug_selector(move || format!("{name}-suggestions"))
            .occlude()
            .popover_style(cx)
            .mt_1()
            .p_1()
            .min_w(scaled(260., cx))
            .max_w(scaled(440., cx))
            .children(self.suggestions.iter().enumerate().map(|(ix, address)| {
                let highlighted = ix == self.highlighted;
                let recipient = Recipient::Address(address.clone());
                let name = address.name.clone().filter(|n| !n.trim().is_empty());
                h_flex()
                    .id(("suggestion", ix))
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(highlighted, |this| {
                        this.bg(theme.accent).text_color(theme.accent_foreground)
                    })
                    .when(!highlighted, |this| {
                        this.hover(|s| s.bg(theme.accent.opacity(0.5)))
                    })
                    .child(avatar(&recipient, scaled(20., cx), false, cx))
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .text_sm()
                            .when_some(name, |this, name| {
                                this.child(
                                    div()
                                        .flex_none()
                                        .max_w(relative(0.6))
                                        .truncate()
                                        .child(name),
                                )
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(if highlighted {
                                        theme.accent_foreground
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(address.email.clone()),
                            ),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            // Keep the focus in the field.
                            window.prevent_default();
                            cx.stop_propagation();
                            this.accept_suggestion(ix, window, cx);
                        }),
                    )
            }))
    }
}

impl Render for RecipientField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if !cx.has_active_drag() {
            self.dragging = None;
        }
        if self.placeholder_shown != self.recipients.is_empty() {
            self.placeholder_shown = self.recipients.is_empty();
            let text = if self.placeholder_shown {
                self.placeholder.clone()
            } else {
                SharedString::default()
            };
            self.input
                .update(cx, |input, cx| input.set_placeholder(text, window, cx));
        }
        let focused = self.input.focus_handle(cx).is_focused(window);
        let this = cx.entity().downgrade();
        let height = chip_height(window);
        let chips: Vec<AnyElement> = self
            .recipients
            .iter()
            .enumerate()
            .map(|(ix, recipient)| {
                self.render_chip(ix, recipient, height, cx)
                    .into_any_element()
            })
            .collect();
        let (radius, border, background) = {
            let theme = cx.theme();
            (theme.radius, theme.input, theme.input_background())
        };
        let name = self.name.clone();
        let field = h_flex()
            .id(SharedString::from(format!("{}-field", self.name)))
            .debug_selector(move || format!("{name}-field"))
            .w_full()
            .flex_wrap()
            .items_center()
            .gap_1()
            .min_h(gpui::rems(FIELD_HEIGHT))
            .px(px(1.))
            .py(px(1.))
            .rounded(radius)
            .border_1()
            .border_color(border)
            .bg(background)
            .cursor_text()
            .when(focused, |this| this.focus_ring_style(window, cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    let focus = this.input.focus_handle(cx);
                    window.focus(&focus, cx);
                    // The composer's own focus mustn't take it back.
                    window.prevent_default();
                    this.select(None, cx);
                }),
            )
            .drag_over::<DraggedRecipient>(|style, _, _, cx| style.bg(cx.theme().drop_target))
            .on_drop(cx.listener(|this, drag: &DraggedRecipient, _, cx| {
                this.drop_recipient(drag, None, cx);
            }))
            .children(chips)
            .child(
                div()
                    .flex_1()
                    .min_w(scaled(96., cx))
                    .h(height)
                    .flex()
                    .items_center()
                    .child(
                        Input::new(&self.input)
                            .small()
                            .appearance(false)
                            .h(height)
                            .on_paste(move |item, window, cx| {
                                let Some(text) = item.text() else {
                                    return false;
                                };
                                this.update(cx, |this, cx| this.paste(&text, window, cx))
                                    .unwrap_or(false)
                            }),
                    ),
            );
        let open = !self.suggestions.is_empty() && focused;
        div()
            .relative()
            .w_full()
            // Keys the text field would otherwise handle (or pass on to the composer).
            .capture_action(cx.listener(|this, action: &input::Enter, window, cx| {
                if action.secondary {
                    return;
                }
                if !this.suggestions.is_empty() {
                    cx.stop_propagation();
                    this.accept_suggestion(this.highlighted, window, cx);
                } else if !this.text(cx).trim().is_empty() {
                    cx.stop_propagation();
                    this.commit_text(window, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::IndentInline, window, cx| {
                if !this.suggestions.is_empty() {
                    cx.stop_propagation();
                    this.accept_suggestion(this.highlighted, window, cx);
                } else {
                    // On to the next field; the text becomes chips first.
                    this.commit_text(window, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::MoveDown, _, cx| {
                if !this.suggestions.is_empty() {
                    cx.stop_propagation();
                    this.move_highlight(true, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::MoveUp, _, cx| {
                if !this.suggestions.is_empty() {
                    cx.stop_propagation();
                    this.move_highlight(false, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::Escape, _, cx| {
                if !this.suggestions.is_empty() {
                    cx.stop_propagation();
                    this.close_suggestions();
                    cx.notify();
                } else if this.selected.is_some() {
                    cx.stop_propagation();
                    this.select(None, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::Backspace, _, cx| {
                if let Some(ix) = this.selected {
                    cx.stop_propagation();
                    this.remove(ix, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::Delete, _, cx| {
                if let Some(ix) = this.selected {
                    cx.stop_propagation();
                    this.remove(ix, cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::MoveRight, _, cx| {
                if let Some(ix) = this.selected {
                    cx.stop_propagation();
                    this.select((ix + 1 < this.recipients.len()).then_some(ix + 1), cx);
                }
            }))
            .capture_action(cx.listener(|this, _: &input::MoveLeft, _, cx| {
                if let Some(ix) = this.selected {
                    cx.stop_propagation();
                    this.select(Some(ix.saturating_sub(1)), cx);
                }
            }))
            // The text field passes these on at its start: on to the chips.
            .on_action(cx.listener(|this, _: &input::Backspace, _, cx| {
                if this.text(cx).is_empty() && !this.recipients.is_empty() {
                    this.select(Some(this.recipients.len() - 1), cx);
                }
            }))
            .on_action(cx.listener(|this, _: &input::MoveLeft, _, cx| {
                if !this.recipients.is_empty() {
                    this.select(Some(this.recipients.len() - 1), cx);
                }
            }))
            .child(field)
            .when(open, |this| {
                this.child(
                    div().absolute().top(relative(1.)).left_0().child(
                        deferred(
                            anchored()
                                .snap_to_window_with_margin(px(8.))
                                .child(self.render_suggestions(cx)),
                        )
                        .with_priority(1),
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_splits_like_address_lists() {
        assert_eq!(
            split_typed("anna@example.com"),
            (vec![], "anna@example.com")
        );
        assert_eq!(
            split_typed("a@x.example, b@y.example; c"),
            (vec!["a@x.example", " b@y.example"], " c")
        );
        // Separators in quotes and angle brackets don't split.
        assert_eq!(
            split_typed("\"Doe, John\" <j@x.example>, \"Esc \\\" ,\" <e@x.example>"),
            (
                vec!["\"Doe, John\" <j@x.example>"],
                " \"Esc \\\" ,\" <e@x.example>"
            )
        );
        assert_eq!(
            split_typed("Ünï <ü@x.example>;"),
            (vec!["Ünï <ü@x.example>"], "")
        );
    }

    #[test]
    fn entries_become_recipients() {
        assert_eq!(Recipient::parse("  "), None);
        assert_eq!(
            Recipient::parse(" Anna <anna@example.com> "),
            Some(Recipient::Address(Address::new(
                Some("Anna"),
                "anna@example.com"
            )))
        );
        assert_eq!(
            Recipient::parse("nobody"),
            Some(Recipient::Invalid("nobody".into()))
        );
        let anna = Recipient::parse("Anna <anna@example.com>").unwrap();
        assert_eq!(anna.label(), "Anna");
        assert_eq!(anna.detail(), "Anna <anna@example.com>");
        assert_eq!(recipient_initials(&anna), "A");
        let bob = Recipient::parse("bob.miller@example.org").unwrap();
        assert_eq!(bob.label(), "bob.miller@example.org");
        assert_eq!(recipient_initials(&bob), "B");
        let both = Recipient::parse("Anna Example <a@x.example>").unwrap();
        assert_eq!(recipient_initials(&both), "AE");
        assert!(!format!("{anna:?}").contains("anna"));
    }
}

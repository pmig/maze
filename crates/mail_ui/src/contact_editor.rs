//! The contact editor (ADR-024): the dialog behind Edit on a version of a contact in
//! the Address Book. It has the fields Maze saves to Google or Microsoft: the name, the
//! email addresses (the first is the contact's own), the phone numbers with their kind
//! (mobile, work, …), the organization and the title. What it doesn't show (postal
//! addresses, notes, …) stays as it is.
//!
//! Saving goes to the server first ([`MailSource::save_contact`], off the main thread):
//! the dialog says so and closes once the server accepted the change
//! ([`ContactEditorEvent::Saved`]). When it didn't (offline, the server refused), the
//! dialog stays open with the reason. When the contact was changed elsewhere in the
//! meantime, the fields show the server's current version, to change again.

use std::sync::Arc;

use gpui::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Task, Window, div, prelude::FluentBuilder as _,
};
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use mazemail_types::Address;
use ui::assets::IconName;
use ui::theme::scaled;

use crate::source::{ContactEdit, ContactError, ContactPhone, MailSource};

/// What the editor tells the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContactEditorEvent {
    /// The server accepted the change: the contact's address (the first one), to
    /// select it in the Address Book.
    Saved(SharedString),
}

/// A phone number's two fields.
struct PhoneFields {
    kind: Entity<InputState>,
    number: Entity<InputState>,
}

/// The content of the Edit Contact dialog. See the [module docs](self).
pub struct ContactEditor {
    source: Arc<dyn MailSource>,
    /// The book written to.
    book: SharedString,
    /// The entry changed; `None` for a new contact.
    contact: Option<i64>,
    /// The book for people: "My Contacts of Gmail".
    book_name: SharedString,
    /// The contact's address when the editor opened, to find it again after a conflict.
    opened_as: Option<SharedString>,
    pub(crate) name: Entity<InputState>,
    pub(crate) emails: Vec<Entity<InputState>>,
    phones: Vec<PhoneFields>,
    pub(crate) organization: Entity<InputState>,
    pub(crate) title: Entity<InputState>,
    saving: Option<Task<()>>,
    error: Option<SharedString>,
}

impl std::fmt::Debug for ContactEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The fields are personal data: never in logs.
        f.debug_struct("ContactEditor")
            .field("contact", &self.contact)
            .field("saving", &self.saving.is_some())
            .finish_non_exhaustive()
    }
}

impl EventEmitter<ContactEditorEvent> for ContactEditor {}

impl ContactEditor {
    /// An editor that starts from `edit`, saving to the book named `book_name`.
    pub fn new(
        source: Arc<dyn MailSource>,
        edit: &ContactEdit,
        book_name: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<'_, Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let mut this = Self {
            source,
            book: edit.book.clone(),
            contact: edit.contact,
            book_name,
            opened_as: edit.emails.first().cloned(),
            name: input("Name", window, cx),
            emails: Vec::new(),
            phones: Vec::new(),
            organization: input("Organization", window, cx),
            title: input("Title", window, cx),
            saving: None,
            error: None,
        };
        this.fill(edit, window, cx);
        this
    }

    /// Shows `edit` in the fields.
    fn fill(&mut self, edit: &ContactEdit, window: &mut Window, cx: &mut Context<'_, Self>) {
        let set = |input: &Entity<InputState>,
                   value: Option<&SharedString>,
                   window: &mut Window,
                   cx: &mut Context<'_, Self>| {
            let value = value.cloned().unwrap_or_default();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        };
        set(&self.name, edit.name.as_ref(), window, cx);
        set(&self.organization, edit.organization.as_ref(), window, cx);
        set(&self.title, edit.title.as_ref(), window, cx);
        self.emails.clear();
        for email in &edit.emails {
            let input = self.email_input(window, cx);
            set(&input, Some(email), window, cx);
            self.emails.push(input);
        }
        if self.emails.is_empty() {
            let input = self.email_input(window, cx);
            self.emails.push(input);
        }
        self.phones.clear();
        for phone in &edit.phones {
            let fields = self.phone_fields(window, cx);
            set(&fields.kind, phone.kind.as_ref(), window, cx);
            set(&fields.number, Some(&phone.number), window, cx);
            self.phones.push(fields);
        }
        cx.notify();
    }

    fn email_input(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> Entity<InputState> {
        cx.new(|cx| InputState::new(window, cx).placeholder("anna@example.com"))
    }

    fn phone_fields(&self, window: &mut Window, cx: &mut Context<'_, Self>) -> PhoneFields {
        PhoneFields {
            kind: cx.new(|cx| InputState::new(window, cx).placeholder("mobile")),
            number: cx.new(|cx| InputState::new(window, cx).placeholder("+43 660 1234567")),
        }
    }

    /// The contact as the fields say: empty fields left out, everything trimmed.
    pub fn edit(&self, cx: &gpui::App) -> ContactEdit {
        let text = |input: &Entity<InputState>| -> Option<SharedString> {
            let value = input.read(cx).value();
            let value = value.trim();
            (!value.is_empty()).then(|| SharedString::from(value.to_owned()))
        };
        ContactEdit {
            book: self.book.clone(),
            contact: self.contact,
            name: text(&self.name),
            emails: self.emails.iter().filter_map(text).collect(),
            phones: self
                .phones
                .iter()
                .filter_map(|phone| {
                    Some(ContactPhone {
                        kind: text(&phone.kind),
                        number: text(&phone.number)?,
                    })
                })
                .collect(),
            organization: text(&self.organization),
            title: text(&self.title),
        }
    }

    /// Adds an empty address field and focuses it.
    pub fn add_email(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let input = self.email_input(window, cx);
        window.focus(&input.focus_handle(cx), cx);
        self.emails.push(input);
        cx.notify();
    }

    /// Removes address field `ix` (one always stays).
    pub fn remove_email(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if self.emails.len() > 1 && ix < self.emails.len() {
            self.emails.remove(ix);
            cx.notify();
        }
    }

    /// Adds an empty phone number and focuses it.
    pub fn add_phone(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let fields = self.phone_fields(window, cx);
        window.focus(&fields.number.focus_handle(cx), cx);
        self.phones.push(fields);
        cx.notify();
    }

    /// Removes phone number `ix`.
    pub fn remove_phone(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if ix < self.phones.len() {
            self.phones.remove(ix);
            cx.notify();
        }
    }

    /// Whether a save is on its way to the server.
    pub fn is_saving(&self) -> bool {
        self.saving.is_some()
    }

    /// Why the last save failed, if it did.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    /// Saves the contact to the server, or says why it can't be saved. Does nothing
    /// while a save is on its way.
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.saving.is_some() {
            return;
        }
        let edit = self.edit(cx);
        if let Err(reason) = check(&edit) {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        self.error = None;
        let email = edit.emails[0].clone();
        let source = self.source.clone();
        self.saving = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { source.save_contact(edit) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.saved(result, email, window, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn saved(
        &mut self,
        result: Result<(), ContactError>,
        email: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.saving = None;
        match result {
            Ok(()) => cx.emit(ContactEditorEvent::Saved(email)),
            Err(ContactError::Conflict) => {
                if let Some(current) = self.current_version(cx) {
                    self.fill(&current, window, cx);
                }
                self.error = Some(
                    "The contact was changed elsewhere in the meantime. The fields show its \
                     current version now: make your change again and save."
                        .into(),
                );
            }
            Err(error) => self.error = Some(error.to_string().into()),
        }
        cx.notify();
    }

    /// The stored version of the edited entry (after a conflict, the server's).
    fn current_version(&self, cx: &gpui::App) -> Option<ContactEdit> {
        let contact = self.contact?;
        let addresses = self.opened_as.iter().cloned().chain(self.edit(cx).emails);
        addresses
            .filter_map(|email| self.source.contact(&email).ok().flatten())
            .flat_map(|detail| detail.versions)
            .find(|version| version.id == contact && version.book == self.book)
            .map(|version| ContactEdit::of(&version))
    }

    fn render_email(&self, ix: usize, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let removable = self.emails.len() > 1;
        h_flex()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .debug_selector(move || format!("contact-editor-email-{ix}"))
                    .child(Input::new(&self.emails[ix]).small()),
            )
            .when(removable, |row| {
                row.child(
                    Button::new(("contact-editor-remove-email", ix))
                        .debug_selector(move || format!("contact-editor-remove-email-{ix}"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Remove this address")
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_email(ix, cx))),
                )
            })
    }

    fn render_phone(&self, ix: usize, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let phone = &self.phones[ix];
        h_flex()
            .gap_1()
            .child(
                div()
                    .w(scaled(110., cx))
                    .debug_selector(move || format!("contact-editor-phone-kind-{ix}"))
                    .child(Input::new(&phone.kind).small()),
            )
            .child(
                div()
                    .flex_1()
                    .debug_selector(move || format!("contact-editor-phone-{ix}"))
                    .child(Input::new(&phone.number).small()),
            )
            .child(
                Button::new(("contact-editor-remove-phone", ix))
                    .debug_selector(move || format!("contact-editor-remove-phone-{ix}"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Remove this number")
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_phone(ix, cx))),
            )
    }
}

/// Whether `edit` can be saved; the reason for people if not. Maze lists contacts by
/// address, so one is needed, and every address has to be one.
fn check(edit: &ContactEdit) -> Result<(), SharedString> {
    if edit.emails.is_empty() {
        return Err("Enter an email address: Maze lists contacts by their address.".into());
    }
    for email in &edit.emails {
        let bare = Address::parse(email).is_ok_and(|a| a.name.is_none() && a.email == **email);
        if !bare {
            return Err(format!("“{email}” isn't an email address.").into());
        }
    }
    Ok(())
}

impl Render for ContactEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let label = |text: &'static str| div().text_xs().text_color(muted).child(text);
        let emails: Vec<_> = (0..self.emails.len())
            .map(|ix| self.render_email(ix, cx))
            .collect();
        let phones: Vec<_> = (0..self.phones.len())
            .map(|ix| self.render_phone(ix, cx))
            .collect();
        let note: SharedString = match &self.saving {
            Some(_) => format!("Saving to {}…", self.book_name).into(),
            None => format!(
                "Saved to {} first: Maze shows the change once it's there.",
                self.book_name
            )
            .into(),
        };
        v_flex()
            .id("contact-editor")
            .max_h(scaled(520., cx))
            .overflow_y_scroll()
            .gap_2()
            .text_sm()
            .child(
                div()
                    .debug_selector(|| "contact-editor-note".to_owned())
                    .text_xs()
                    .text_color(muted)
                    .child(note),
            )
            .child(label("Name"))
            .child(
                div()
                    .debug_selector(|| "contact-editor-name".to_owned())
                    .child(Input::new(&self.name).small()),
            )
            .child(label("Email addresses (the first is the contact's own)"))
            .children(emails)
            .child(
                h_flex().child(
                    Button::new("contact-editor-add-email")
                        .debug_selector(|| "contact-editor-add-email".to_owned())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label("Add Address")
                        .on_click(cx.listener(|this, _, window, cx| this.add_email(window, cx))),
                ),
            )
            .child(label("Phone numbers"))
            .children(phones)
            .child(
                h_flex().child(
                    Button::new("contact-editor-add-phone")
                        .debug_selector(|| "contact-editor-add-phone".to_owned())
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .label("Add Phone Number")
                        .on_click(cx.listener(|this, _, window, cx| this.add_phone(window, cx))),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .gap_1()
                            .child(label("Organization"))
                            .child(
                                div()
                                    .debug_selector(|| "contact-editor-organization".to_owned())
                                    .child(Input::new(&self.organization).small()),
                            ),
                    )
                    .child(
                        v_flex().flex_1().gap_1().child(label("Title")).child(
                            div()
                                .debug_selector(|| "contact-editor-title".to_owned())
                                .child(Input::new(&self.title).small()),
                        ),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "contact-editor-error".to_owned())
                        .text_color(danger)
                        .child(error),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(emails: &[&str]) -> ContactEdit {
        ContactEdit {
            book: "google:fake-1".into(),
            contact: Some(1),
            name: Some("Anna".into()),
            emails: emails
                .iter()
                .map(|e| SharedString::from(e.to_string()))
                .collect(),
            phones: Vec::new(),
            organization: None,
            title: None,
        }
    }

    #[test]
    fn needs_one_address_and_only_addresses() {
        assert!(check(&edit(&["anna@example.com", "a.b+c@example.org"])).is_ok());
        assert!(check(&edit(&[])).is_err());
        for bad in [
            "anna",
            "anna@",
            "@example.com",
            "Anna <anna@example.com>",
            "a b@x.com",
        ] {
            let reason = check(&edit(&[bad])).unwrap_err();
            assert!(reason.contains(bad), "{bad}: {reason}");
        }
    }
}

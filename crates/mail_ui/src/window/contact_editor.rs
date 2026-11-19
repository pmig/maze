//! The Edit Contact dialog in the window (ADR-024): Edit on a contact's version in the
//! Address Book opens it; it closes once the server accepted the change, and the
//! Address Book then shows the saved contact.

use gpui::{
    App, AppContext as _, ClickEvent, Context, Entity, Focusable as _, InteractiveElement as _,
    ParentElement as _, SharedString, Styled as _, Window,
};
use gpui_kit::component::{
    Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
};

use super::MailWindow;
use crate::contact_editor::{ContactEditor, ContactEditorEvent};
use crate::source::ContactEdit;

impl MailWindow {
    /// Opens the Edit Contact dialog on `edit` (Add Contact for a new one), saving to the
    /// book called `book_name`.
    pub(super) fn open_contact_editor(
        &mut self,
        edit: &ContactEdit,
        book_name: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let source = self.source.clone();
        let form = cx.new(|cx| ContactEditor::new(source, edit, book_name, window, cx));
        let subscription =
            cx.subscribe_in(&form, window, |this, _, event, window, cx| match event {
                ContactEditorEvent::Saved(email) => this.contact_saved(email.clone(), window, cx),
            });
        self.contact_editor = Some((form.clone(), subscription));
        let focus = form.read(cx).name.focus_handle(cx);
        let this = cx.weak_entity();
        let title = if edit.contact.is_some() {
            "Edit Contact"
        } else {
            "Add Contact"
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let saving = form.read(cx).is_saving();
            let save = {
                let form = form.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    form.update(cx, |form, cx| form.submit(window, cx));
                }
            };
            // Enter saves too; the dialog closes once the server accepted the change.
            let confirm = {
                let form = form.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    form.update(cx, |form, cx| form.submit(window, cx));
                    false
                }
            };
            // Closed without saving (Escape, the close button): a save still on its
            // way finishes, but doesn't close anything any more.
            let closed = {
                let this = this.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    this.update(cx, |this, _| this.contact_editor = None).ok();
                }
            };
            dialog
                .title(title)
                .on_ok(confirm)
                .on_close(closed)
                .width(ui::theme::scaled(520., cx))
                .child(form.clone())
                .footer(
                    h_flex().w_full().justify_end().gap_2().child(
                        Button::new("save-contact")
                            .debug_selector(|| "save-contact".to_owned())
                            .primary()
                            .small()
                            .loading(saving)
                            .disabled(saving)
                            .label(if saving { "Saving…" } else { "Save" })
                            .on_click(save),
                    ),
                )
        });
        window.focus(&focus, cx);
    }

    /// The open Edit Contact dialog's form, if one is open.
    pub fn contact_editor(&self) -> Option<&Entity<ContactEditor>> {
        self.contact_editor.as_ref().map(|(form, _)| form)
    }

    /// The server accepted the contact: the dialog closes and the Address Book shows
    /// it, under its (maybe new) address.
    fn contact_saved(
        &mut self,
        email: SharedString,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.contact_editor = None;
        window.close_dialog(cx);
        self.address_book
            .update(cx, |book, cx| book.reload_selecting(Some(email), cx));
    }
}

//! The Edit Contact dialog (ADR-024): Edit on a version in the Address Book opens it
//! with the version's fields; Save hands the change to the source and closes once it
//! was saved; a failure, a conflict or an address that isn't one keeps it open.

use gpui::{Modifiers, TestAppContext};

use super::*;
use crate::contact_editor::ContactEditor;
use crate::source::{ContactEdit, ContactError, ContactPhone};

/// A window with the app's Root (dialogs need its layer), showing the Address Book
/// with Anna selected: My Contacts, Microsoft 365 and Collected have her.
fn open_address_book(
    cx: &mut TestAppContext,
) -> (
    Entity<MailWindow>,
    Arc<FakeMailSource>,
    &mut VisualTestContext,
) {
    cx.update(|cx| {
        ui::init(cx);
        crate::init(cx);
    });
    let fake = Arc::new(FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)));
    let source: Arc<dyn MailSource> = fake.clone();
    let mut view = None;
    let window = cx.add_window(|window, cx| {
        let mail = cx.new(|cx| MailWindow::new(source, &MailWindowOptions::default(), window, cx));
        view = Some(mail.clone());
        gpui_kit::base::Root::new(mail, window, cx)
    });
    let view = view.unwrap();
    let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
    cx.update(|window, _| window.activate_window());
    cx.run_until_parked();
    click(cx, "space-contacts");
    (view, fake, cx)
}

fn click(cx: &mut VisualTestContext, selector: &'static str) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} is on screen"));
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

/// Opens the editor on version `ix` of the selected contact and waits for the dialog's
/// entrance animation to end, so clicks land where its buttons are drawn.
fn open_editor(
    view: &Entity<MailWindow>,
    ix: usize,
    cx: &mut VisualTestContext,
) -> Entity<ContactEditor> {
    let selector: &'static str = Box::leak(format!("contact-edit-{ix}").into_boxed_str());
    click(cx, selector);
    let form = editor(view, cx).expect("Edit opens the editor");
    let mut last = None;
    for _ in 0..100 {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let now = cx.debug_bounds("contact-editor-name");
        if now.is_some() && now == last {
            return form;
        }
        last = now;
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the dialog keeps moving");
}

fn shown(cx: &mut VisualTestContext, selector: &'static str) -> bool {
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.debug_bounds(selector).is_some()
}

fn editor(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> Option<Entity<ContactEditor>> {
    view.read_with(cx, |w, _| w.contact_editor().cloned())
}

fn fields(form: &Entity<ContactEditor>, cx: &mut VisualTestContext) -> ContactEdit {
    form.read_with(cx, |form, cx| form.edit(cx))
}

fn type_into(
    form: &Entity<ContactEditor>,
    field: impl Fn(&ContactEditor) -> Entity<gpui_kit::component::input::InputState>,
    text: &str,
    cx: &mut VisualTestContext,
) {
    let input = form.read_with(cx, |form, _| field(form));
    cx.update(|window, cx| {
        input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    });
    cx.run_until_parked();
}

fn error(form: &Entity<ContactEditor>, cx: &mut VisualTestContext) -> Option<String> {
    form.read_with(cx, |form, _| form.error().map(ToString::to_string))
}

#[gpui::test]
fn edit_opens_the_version_and_save_hands_the_change_over(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_address_book(cx);
    assert!(editor(&view, cx).is_none());

    // Edit on My Contacts' version: the dialog shows its fields.
    let form = open_editor(&view, 0, cx);
    assert!(shown(cx, "contact-editor-name"));
    let before = fields(&form, cx);
    assert_eq!(before.book.as_ref(), "google:fake-1");
    assert_eq!(before.name.as_deref(), Some("Anna Example"));
    assert_eq!(before.emails, ["anna@example.com"]);
    assert_eq!(
        before.phones,
        [ContactPhone {
            kind: Some("cell".into()),
            number: "+43 660 1234567".into(),
        }]
    );
    assert_eq!(before.organization.as_deref(), Some("Example GmbH"));
    assert_eq!(before.title.as_deref(), Some("Head of Research"));

    // A new name, a second address, no phone number.
    type_into(&form, |f| f.name.clone(), "  Anna M. Example ", cx);
    click(cx, "contact-editor-add-email");
    type_into(&form, |f| f.emails[1].clone(), "anna@work.example", cx);
    click(cx, "contact-editor-remove-phone-0");
    click(cx, "save-contact");

    let saves = fake.contact_saves();
    assert_eq!(saves.len(), 1);
    let saved = &saves[0];
    assert_eq!(saved.book, before.book);
    assert_eq!(saved.contact, before.contact);
    assert_eq!(saved.name.as_deref(), Some("Anna M. Example"));
    assert_eq!(saved.emails, ["anna@example.com", "anna@work.example"]);
    assert!(saved.phones.is_empty());
    assert_eq!(saved.organization, before.organization);
    // Saved: the dialog is gone, Anna is still selected.
    assert!(editor(&view, cx).is_none());
    assert!(!shown(cx, "contact-editor-name"));
    let selected = view.read_with(cx, |w, cx| {
        w.address_book()
            .read(cx)
            .selected_row()
            .map(|row| row.email.to_string())
    });
    assert_eq!(selected.as_deref(), Some("anna@example.com"));
}

#[gpui::test]
fn a_failed_save_keeps_the_dialog_open_with_the_reason(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_address_book(cx);
    let form = open_editor(&view, 1, cx);
    assert_eq!(fields(&form, cx).book.as_ref(), "microsoft:fake-3");

    // Not an address, saved with Enter: nothing goes to the server.
    type_into(&form, |f| f.emails[0].clone(), "anna", cx);
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert!(fake.contact_saves().is_empty());
    assert!(
        error(&form, cx)
            .unwrap()
            .contains("“anna” isn't an email address")
    );
    assert!(shown(cx, "contact-editor-error"));

    // Offline: the server's reason, still open, the typed fields kept.
    type_into(&form, |f| f.emails[0].clone(), "anna@example.com", cx);
    type_into(&form, |f| f.title.clone(), "CEO", cx);
    let offline = "Maze can't reach the contacts server right now";
    fake.fail_contact_saves(Some(ContactError::Failed(offline.to_owned())));
    click(cx, "save-contact");
    assert_eq!(error(&form, cx).as_deref(), Some(offline));
    assert!(editor(&view, cx).is_some());
    assert_eq!(fields(&form, cx).title.as_deref(), Some("CEO"));

    // Changed elsewhere: the fields show the stored (the server's) version again.
    fake.fail_contact_saves(Some(ContactError::Conflict));
    click(cx, "save-contact");
    assert!(error(&form, cx).unwrap().contains("changed elsewhere"));
    assert_eq!(fields(&form, cx).title.as_deref(), Some("Head of Research"));

    // Saving again works, and closes the dialog.
    fake.fail_contact_saves(None);
    click(cx, "save-contact");
    assert_eq!(fake.contact_saves().len(), 1);
    assert!(editor(&view, cx).is_none());
}

#[gpui::test]
fn escape_closes_the_editor_without_saving(cx: &mut TestAppContext) {
    let (view, fake, cx) = open_address_book(cx);
    let form = open_editor(&view, 0, cx);
    type_into(&form, |f| f.name.clone(), "Someone Else", cx);
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(editor(&view, cx).is_none());
    assert!(!shown(cx, "contact-editor-name"));
    assert!(fake.contact_saves().is_empty());
}

#[gpui::test]
fn read_only_versions_have_no_edit_button(cx: &mut TestAppContext) {
    let (view, _, cx) = open_address_book(cx);
    // Anna's third version is the collected one.
    let editable: Vec<bool> = view.read_with(cx, |w, cx| {
        let book = w.address_book().read(cx);
        let detail = book.detail().unwrap();
        detail.versions.iter().map(|v| v.editable).collect()
    });
    assert_eq!(editable, [true, true, false]);
    assert!(shown(cx, "contact-edit-1"));
    assert!(!shown(cx, "contact-edit-2"));
}

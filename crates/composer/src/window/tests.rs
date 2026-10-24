use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Mutex;

use gpui::{TestAppContext, VisualTestContext};
use mazemail_types::MessageId;
use mazemail_types::compose::DraftOrigin;

use super::*;
use crate::backend::{BoxFuture, ComposeError};

/// Records what the composer asks for.
#[derive(Default)]
struct FakeBackend {
    saves: Mutex<Vec<(Option<DraftId>, Draft)>>,
    deletes: Mutex<Vec<DraftId>>,
    sends: Mutex<Vec<(Option<DraftId>, Draft)>>,
    send_error: Mutex<Option<String>>,
    no_identities: bool,
}

impl FakeBackend {
    fn saves(&self) -> Vec<(Option<DraftId>, Draft)> {
        self.saves.lock().unwrap().clone()
    }
    fn sends(&self) -> Vec<(Option<DraftId>, Draft)> {
        self.sends.lock().unwrap().clone()
    }
    fn deletes(&self) -> Vec<DraftId> {
        self.deletes.lock().unwrap().clone()
    }
}

impl ComposeBackend for FakeBackend {
    fn identities(&self) -> Vec<IdentityChoice> {
        if self.no_identities {
            return Vec::new();
        }
        vec![
            IdentityChoice {
                id: IdentityId(1),
                name: Some("Philip".into()),
                email: "philip@example.org".into(),
                account: "Work".into(),
            },
            IdentityChoice {
                id: IdentityId(2),
                name: None,
                email: "me@distr.example".into(),
                account: "Work".into(),
            },
        ]
    }

    fn save_draft(
        &self,
        id: Option<DraftId>,
        draft: Draft,
    ) -> BoxFuture<Result<DraftId, ComposeError>> {
        let mut saves = self.saves.lock().unwrap();
        saves.push((id, draft));
        let id = id.unwrap_or(DraftId(41 + saves.len() as i64));
        Box::pin(async move { Ok(id) })
    }

    fn delete_draft(&self, id: DraftId) -> BoxFuture<Result<(), ComposeError>> {
        self.deletes.lock().unwrap().push(id);
        Box::pin(async { Ok(()) })
    }

    fn send(
        &self,
        id: Option<DraftId>,
        draft: Draft,
    ) -> BoxFuture<Result<SendOutcome, ComposeError>> {
        self.sends.lock().unwrap().push((id, draft));
        let error = self.send_error.lock().unwrap().clone();
        Box::pin(async move {
            match error {
                Some(message) => Err(ComposeError::new(message)),
                None => Ok(SendOutcome::Sent),
            }
        })
    }
}

fn open(
    cx: &mut TestAppContext,
    backend: Arc<FakeBackend>,
    options: ComposerOptions,
) -> (Entity<Composer>, &mut VisualTestContext) {
    cx.update(|cx| {
        ui::init(cx);
        // The composer's keys live in the default keymap (unknown actions of other
        // crates are skipped).
        let _ = ui::keymap::bind_keymap(ui::keymap::DEFAULT_MACOS_KEYMAP, cx);
        crate::init(cx);
    });
    let backend: Arc<dyn ComposeBackend> = backend;
    let (view, cx) = cx.add_window_view(|window, cx| Composer::new(backend, options, window, cx));
    cx.run_until_parked();
    (view, cx)
}

fn options() -> ComposerOptions {
    let mut options = ComposerOptions::new(Draft::default());
    options.autosave_after = Duration::from_secs(1);
    options
}

#[gpui::test]
fn sending_without_recipients_shows_an_error(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend.clone(), options());
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |c, _| c.error()),
        Some("Add at least one recipient.".into())
    );
    assert!(backend.sends().is_empty());
    assert!(!view.read_with(cx, |c, _| c.is_closed()));
}

#[gpui::test]
fn an_invalid_address_is_named(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend.clone(), options());
    // The To field has the focus in a new message.
    cx.simulate_input("anna@example.com, nobody");
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    let error = view.read_with(cx, |c, _| c.error()).unwrap();
    assert!(error.contains("“nobody”"), "{error}");
    assert!(backend.sends().is_empty());
}

#[gpui::test]
fn cmd_enter_sends_the_draft_and_closes(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.in_reply_to = Some("orig@example.com".into());
    options.draft.references = vec!["root@example.com".into(), "orig@example.com".into()];
    options.draft.origin = Some(DraftOrigin::Reply(MessageId(7)));
    options.draft.body_markdown = "\n\n> quoted".into();
    let (view, cx) = open(cx, backend.clone(), options);
    let events: Rc<RefCell<Vec<ComposerEvent>>> = Rc::default();
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&view, move |_, event: &ComposerEvent, _| {
            events.borrow_mut().push(event.clone());
        })
        .detach();
    });

    cx.simulate_input("Anna <anna@example.com>");
    view.update_in(cx, |c, window, cx| {
        c.subject
            .update(cx, |s, cx| s.set_value("Re: Plans", window, cx));
        c.body.update(cx, |b, cx| {
            b.set_value("Sounds good!\n\n> quoted", window, cx);
        });
    });
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();

    let sends = backend.sends();
    assert_eq!(sends.len(), 1);
    let (id, draft) = &sends[0];
    assert_eq!(*id, None);
    assert_eq!(draft.identity, Some(IdentityId(1)), "the first identity");
    assert_eq!(
        draft.to,
        vec![Address::new(Some("Anna"), "anna@example.com")]
    );
    assert_eq!(draft.subject, "Re: Plans");
    assert_eq!(draft.body_markdown, "Sounds good!\n\n> quoted");
    assert_eq!(draft.in_reply_to.as_deref(), Some("orig@example.com"));
    assert_eq!(draft.references.len(), 2);
    assert_eq!(draft.origin, Some(DraftOrigin::Reply(MessageId(7))));
    assert_eq!(
        *events.borrow(),
        vec![ComposerEvent::Sent(SendOutcome::Sent)]
    );
    assert!(view.read_with(cx, |c, _| c.is_closed()));
}

#[gpui::test]
fn a_refused_send_keeps_the_window(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    *backend.send_error.lock().unwrap() = Some("SendAsDenied".into());
    let (view, cx) = open(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    assert_eq!(backend.sends().len(), 1);
    assert_eq!(
        view.read_with(cx, |c, _| c.error()),
        Some("SendAsDenied".into())
    );
    assert!(!view.read_with(cx, |c, _| c.is_closed()));
}

#[gpui::test]
fn no_identity_means_no_sending(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend {
        no_identities: true,
        ..FakeBackend::default()
    });
    let (view, cx) = open(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    assert!(backend.sends().is_empty());
    assert!(view.read_with(cx, |c, _| c.error()).is_some());
}

#[gpui::test]
fn drafts_are_autosaved_after_a_pause(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    assert!(view.read_with(cx, |c, _| c.is_dirty()));
    assert!(backend.saves().is_empty(), "not before the pause");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let saves = backend.saves();
    assert_eq!(saves.len(), 1);
    assert_eq!(saves[0].0, None);
    assert_eq!(saves[0].1.to[0].email, "anna@example.com");
    assert_eq!(view.read_with(cx, |c, _| c.draft_id()), Some(DraftId(42)));
    assert!(!view.read_with(cx, |c, _| c.is_dirty()));

    // The next save updates the same draft.
    cx.simulate_input(", bob@example.com");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let saves = backend.saves();
    assert_eq!(saves.len(), 2);
    assert_eq!(saves[1].0, Some(DraftId(42)));
}

#[gpui::test]
fn blank_drafts_are_not_saved(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (_view, cx) = open(cx, backend.clone(), options());
    cx.simulate_input("a");
    cx.simulate_keystrokes("backspace");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    assert!(backend.saves().is_empty());
}

#[gpui::test]
fn cmd_s_saves_now(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (_view, cx) = open(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    assert_eq!(backend.saves().len(), 1);
}

#[gpui::test]
fn escape_on_a_clean_message_closes_without_asking(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend.clone(), options());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert!(view.read_with(cx, |c, _| c.is_closed()));
    assert!(backend.saves().is_empty() && backend.deletes().is_empty());
}

#[gpui::test]
fn escape_on_changes_asks_and_dont_save_discards(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    // Autosaved as a new draft, then changed again.
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    cx.simulate_input(", bob@example.com");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    assert!(!view.read_with(cx, |c, _| c.is_closed()));
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    assert!(view.read_with(cx, |c, _| c.is_closed()));
    assert_eq!(
        backend.deletes(),
        vec![DraftId(42)],
        "the autosaved draft goes"
    );
}

#[gpui::test]
fn dont_save_keeps_a_draft_that_existed_before(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.to = vec![Address::new(None::<String>, "anna@example.com")];
    options.draft_id = Some(DraftId(5));
    let (view, cx) = open(cx, backend.clone(), options);
    // The body has the focus when there are recipients.
    cx.simulate_input("Hi");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    assert!(view.read_with(cx, |c, _| c.is_closed()));
    assert!(backend.deletes().is_empty());
}

#[gpui::test]
fn save_in_the_prompt_saves_then_closes(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Save");
    cx.run_until_parked();
    assert_eq!(backend.saves().len(), 1);
    assert!(view.read_with(cx, |c, _| c.is_closed()));
}

#[gpui::test]
fn the_close_button_asks_like_escape(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend, options());
    cx.simulate_input("anna@example.com");
    assert!(!cx.simulate_close(), "kept open while asking");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert!(!view.read_with(cx, |c, _| c.is_closed()));
}

#[gpui::test]
fn cc_and_bcc_toggle(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend, options());
    assert!(!view.read_with(cx, |c, _| c.cc_visible()));
    cx.simulate_keystrokes("cmd-shift-c");
    assert!(view.read_with(cx, |c, _| c.cc_visible()));
    // Cc has the focus now.
    cx.simulate_input("carl@example.com");
    let draft = view.read_with(cx, |c, cx| c.draft(cx)).unwrap();
    assert_eq!(draft.cc[0].email, "carl@example.com");
    cx.simulate_keystrokes("cmd-shift-c");
    assert!(!view.read_with(cx, |c, _| c.cc_visible()));

    // A draft with Cc opens with the fields shown.
    let mut options = options();
    options.draft.bcc = vec![Address::new(None::<String>, "b@example.com")];
    let backend = Arc::new(FakeBackend::default());
    let backend: Arc<dyn ComposeBackend> = backend;
    let (view, cx) = cx.add_window_view(|window, cx| Composer::new(backend, options, window, cx));
    assert!(view.read_with(cx, |c, _| c.cc_visible()));
}

#[gpui::test]
fn attachments_join_the_draft(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend, options());
    let file = DraftAttachment {
        path: "/tmp/plan.pdf".into(),
        name: "plan.pdf".into(),
        size: 2048,
    };
    view.update_in(cx, |c, window, cx| {
        c.add_attachments(vec![file.clone(), file.clone()], window, cx);
    });
    let draft = view.read_with(cx, |c, cx| c.draft(cx)).unwrap();
    assert_eq!(draft.attachments, vec![file]);
    assert_eq!(format_size(2048), "2 KB");
    assert_eq!(format_size(3 * 1024 * 1024 / 2), "1.5 MB");
}

#[gpui::test]
fn the_title_follows_the_subject(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend, options());
    assert_eq!(view.read_with(cx, |c, _| c.title.clone()), "New Message");
    view.update_in(cx, |c, window, cx| {
        let focus = c.subject.focus_handle(cx);
        window.focus(&focus, cx);
    });
    cx.simulate_input("Plans");
    assert_eq!(view.read_with(cx, |c, _| c.title.clone()), "Plans");
    assert_eq!(cx.window_title().as_deref(), Some("Plans"));
}

// ---- in a tab of the main window ----------------------------------------------------

/// A window with a pane group, binding the pane actions like Maze's main window does.
struct Panes {
    group: Entity<ui::panes::PaneGroup>,
}

impl Render for Panes {
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        // The pane keys are bound in the main window's `Workspace` context.
        ui::panes::bind_actions(
            div()
                .size_full()
                .key_context("Workspace")
                .child(self.group.clone()),
            &self.group,
        )
    }
}

fn open_tab(
    cx: &mut TestAppContext,
    backend: Arc<FakeBackend>,
    mut options: ComposerOptions,
) -> (
    Entity<Composer>,
    Entity<ui::panes::PaneGroup>,
    &mut VisualTestContext,
) {
    cx.update(|cx| {
        ui::init(cx);
        let _ = ui::keymap::bind_keymap(ui::keymap::DEFAULT_MACOS_KEYMAP, cx);
        crate::init(cx);
    });
    options.in_tab = true;
    let backend: Arc<dyn ComposeBackend> = backend;
    let (panes, cx) = cx.add_window_view(|window, cx| Panes {
        group: cx.new(|cx| ui::panes::PaneGroup::new(None, window, cx)),
    });
    cx.update(|window, _| window.activate_window());
    let group = panes.read_with(cx, |p, _| p.group.clone());
    let composer = group.update_in(cx, |group, window, cx| {
        let composer = cx.new(|cx| Composer::new(backend, options, window, cx));
        group.add_item(Box::new(composer.clone()), true, window, cx);
        composer
    });
    cx.run_until_parked();
    (composer, group, cx)
}

fn tab_count(group: &Entity<ui::panes::PaneGroup>, cx: &mut VisualTestContext) -> usize {
    group.read_with(cx, |g, cx| g.items(cx).len())
}

#[gpui::test]
fn a_tab_is_titled_with_the_drafts_name(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.name = "brave-otter".into();
    let (composer, group, cx) = open_tab(cx, backend.clone(), options);
    let title = |cx: &mut VisualTestContext| {
        group.read_with(cx, |g, cx| {
            let item = g.active_item(cx).unwrap();
            (
                item.tab_title(cx).to_string(),
                item.is_dirty(cx),
                item.serialize(cx),
            )
        })
    };
    assert_eq!(
        title(cx),
        ("brave-otter".into(), false, None),
        "not saved yet"
    );
    // The To field has the focus in a new message, in a tab too.
    cx.simulate_input("anna@example.com");
    assert!(title(cx).1, "a dot while unsaved");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    let (name, dirty, saved) = title(cx);
    assert_eq!(name, "brave-otter");
    assert!(!dirty);
    let saved = saved.expect("restorable once saved");
    assert_eq!(saved.kind, COMPOSE_KIND);
    assert_eq!(saved.title.as_deref(), Some("brave-otter"));
    assert_eq!(saved.data, serde_json::json!({ "draft": 42 }));
    // The name is stored with the draft.
    assert_eq!(backend.saves()[0].1.name, "brave-otter");
    assert_eq!(
        composer.read_with(cx, |c, _| c.name().clone()),
        "brave-otter"
    );
}

#[gpui::test]
fn a_draft_without_a_name_gets_one(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (composer, _, cx) = open_tab(cx, backend, options());
    let name = composer.read_with(cx, |c, _| c.name().to_string());
    assert!(ui::names::is_two_word_name(&name), "{name}");
}

#[gpui::test]
fn closing_a_tab_with_changes_asks_first(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (composer, group, cx) = open_tab(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    cx.executor().advance_clock(Duration::from_secs(2));
    cx.run_until_parked();
    cx.simulate_input(", bob@example.com");
    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    assert!(cx.has_pending_prompt());
    cx.simulate_prompt_answer("Cancel");
    cx.run_until_parked();
    assert_eq!(tab_count(&group, cx), 1, "kept");
    assert!(!composer.read_with(cx, |c, _| c.is_closed()));

    cx.simulate_keystrokes("cmd-w");
    cx.run_until_parked();
    cx.simulate_prompt_answer("Don't Save");
    cx.run_until_parked();
    assert_eq!(tab_count(&group, cx), 0, "the tab closed");
    assert_eq!(backend.deletes(), vec![DraftId(42)]);
}

#[gpui::test]
fn a_clean_tab_closes_without_asking_and_escape_works_too(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (_, group, cx) = open_tab(cx, backend, options());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!cx.has_pending_prompt());
    assert_eq!(tab_count(&group, cx), 0);
}

#[gpui::test]
fn sending_closes_the_tab(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (_, group, cx) = open_tab(cx, backend.clone(), options());
    cx.simulate_input("anna@example.com");
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    assert_eq!(backend.sends().len(), 1);
    assert_eq!(tab_count(&group, cx), 0);
    // The main window's title isn't the composer's.
    assert_ne!(cx.window_title().as_deref(), Some("New Message"));
}

#[gpui::test]
fn open_in_new_window_hands_the_draft_over(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.name = "quiet-heron".into();
    let (composer, group, cx) = open_tab(cx, backend, options);
    let events: Rc<RefCell<Vec<ComposerEvent>>> = Rc::default();
    cx.update(|_, cx| {
        let events = events.clone();
        cx.subscribe(&composer, move |_, event: &ComposerEvent, _| {
            events.borrow_mut().push(event.clone());
        })
        .detach();
    });
    cx.simulate_input("anna@example.com");
    cx.dispatch_action(crate::actions::OpenInNewWindow);
    cx.run_until_parked();
    assert_eq!(tab_count(&group, cx), 0);
    let events = events.borrow();
    let [ComposerEvent::OpenInNewWindow(options)] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert!(!options.in_tab);
    assert_eq!(options.draft.name, "quiet-heron");
    assert_eq!(options.draft.to[0].email, "anna@example.com");
}

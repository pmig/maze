use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Mutex;

use gpui::{TestAppContext, VisualTestContext};
use mazemail_types::MessageId;
use mazemail_types::compose::DraftOrigin;

use super::*;
use crate::actions::{IgnoreSpelling, ToggleSpellingLanguage};
use crate::backend::{BoxFuture, ComposeError};

// gpui-kit binds its editing keys per platform (cmd on macOS, ctrl elsewhere, and a
// few different keys on Linux), so tests press the ones of the platform they run on.
const UNDO: &str = "secondary-z";
#[cfg(target_os = "macos")]
const REDO: &str = "cmd-shift-z";
#[cfg(not(target_os = "macos"))]
const REDO: &str = "ctrl-y";
#[cfg(target_os = "macos")]
const TO_START: &str = "cmd-up";
#[cfg(not(target_os = "macos"))]
const TO_START: &str = "ctrl-home";

/// Records what the composer asks for.
#[derive(Default)]
struct FakeBackend {
    saves: Mutex<Vec<(Option<DraftId>, Draft)>>,
    deletes: Mutex<Vec<DraftId>>,
    sends: Mutex<Vec<(Option<DraftId>, Draft)>>,
    send_error: Mutex<Option<String>>,
    no_identities: bool,
    /// The footer of identity 1's account.
    footer: String,
    /// `compose.support_maze` is off.
    no_maze_line: bool,
    /// The personal spelling dictionary as stored.
    spelling_words: Vec<String>,
    /// Words added with "Add to Dictionary".
    added_words: Mutex<Vec<String>>,
    /// What address completion knows.
    contacts: Vec<Address>,
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
    fn added_words(&self) -> Vec<String> {
        self.added_words.lock().unwrap().clone()
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

    fn footer(&self, identity: Option<IdentityId>) -> String {
        if identity == Some(IdentityId(1)) {
            self.footer.clone()
        } else {
            String::new()
        }
    }

    fn sent_with_maze(&self, _: Option<IdentityId>) -> bool {
        !self.no_maze_line
    }

    fn complete_address(&self, prefix: &str) -> Vec<Address> {
        let prefix = prefix.to_lowercase();
        self.contacts
            .iter()
            .filter(|a| {
                a.email.to_lowercase().starts_with(&prefix)
                    || a.name
                        .as_deref()
                        .is_some_and(|n| n.to_lowercase().starts_with(&prefix))
            })
            .cloned()
            .collect()
    }

    fn spelling_words(&self) -> BoxFuture<Result<Vec<String>, ComposeError>> {
        let words = self.spelling_words.clone();
        Box::pin(async move { Ok(words) })
    }

    fn add_spelling_word(&self, word: String) -> BoxFuture<Result<(), ComposeError>> {
        self.added_words.lock().unwrap().push(word);
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

#[gpui::test]
fn the_maze_line_follows_the_setting_without_a_switch(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend {
        footer: "Acme Inc. · Vienna\n".into(),
        ..FakeBackend::default()
    });
    let (view, cx) = open(cx, backend, options());
    // `compose.support_maze` is on: the footer and the Maze line are previewed under
    // the body, not part of it, and there is nothing to switch it off here.
    assert!(view.read_with(cx, |c, _| c.support_maze()));
    assert_eq!(
        view.read_with(cx, |c, _| c.footer_preview()),
        vec![
            SharedString::from("Acme Inc. · Vienna"),
            SENT_WITH_MAZE.into()
        ]
    );
    assert!(cx.debug_bounds("footer-preview").is_some());
    assert!(cx.debug_bounds("support-maze").is_none(), "no checkbox");
    cx.simulate_input("anna@example.com");
    let body = view.read_with(cx, |c, cx| c.draft(cx).unwrap().body_markdown);
    assert!(!body.contains("Sent with") && !body.contains("Acme"));
}

#[gpui::test]
fn without_the_setting_there_is_no_maze_line(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend {
        no_maze_line: true,
        ..FakeBackend::default()
    });
    let (view, cx) = open(cx, backend, options());
    assert!(!view.read_with(cx, |c, _| c.support_maze()));
    // No footer configured, no Maze line: no preview.
    assert!(view.read_with(cx, |c, _| c.footer_preview()).is_empty());
    assert!(cx.debug_bounds("footer-preview").is_none());
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

// ---- the body (a prose-configured code editor) --------------------------------------

/// A composer with a recipient, so the body has the focus, and `body` as its text.
fn open_body<'a>(
    cx: &'a mut TestAppContext,
    body: &str,
) -> (Entity<Composer>, &'a mut VisualTestContext) {
    let mut options = options();
    options.draft.to = vec![Address::new(None::<String>, "anna@example.com")];
    options.draft.body_markdown = body.to_owned();
    open(cx, Arc::new(FakeBackend::default()), options)
}

fn body_text(view: &Entity<Composer>, cx: &mut VisualTestContext) -> String {
    view.read_with(cx, |c, cx| c.body.read(cx).value().to_string())
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, cx| window.draw(cx).clear(cx));
}

#[gpui::test]
fn the_body_types_like_a_plain_text_field(cx: &mut TestAppContext) {
    let (view, cx) = open_body(cx, "");
    // No closing brackets or quotes are added, umlauts and emoji go in as typed.
    cx.simulate_input("Don't (see \"this\") [x] {y} – Grüße, Jänner 🎉");
    assert_eq!(
        body_text(&view, cx),
        "Don't (see \"this\") [x] {y} – Grüße, Jänner 🎉"
    );
    assert!(view.read_with(cx, |c, _| c.is_dirty()));

    // Enter between brackets is a plain line break, not a code block.
    view.update_in(cx, |c, window, cx| {
        c.body.update(cx, |b, cx| b.set_value("f()", window, cx));
        c.body.update(cx, |b, cx| b.set_selected_range(2..2, cx));
    });
    cx.simulate_keystrokes("enter");
    assert_eq!(body_text(&view, cx), "f(\n)");

    // Tab indents in the body, as it did in the plain text field.
    view.update_in(cx, |c, window, cx| {
        c.body.update(cx, |b, cx| b.set_value("item", window, cx));
        c.body.update(cx, |b, cx| b.set_selected_range(0..0, cx));
    });
    cx.simulate_keystrokes("tab");
    assert_eq!(body_text(&view, cx), "  item");
    cx.simulate_keystrokes("shift-tab");
    assert_eq!(body_text(&view, cx), "item");
}

#[gpui::test]
fn enter_keeps_the_indentation_of_the_line(cx: &mut TestAppContext) {
    // The one editing difference to the plain text field: a new line starts at the
    // indentation of the one before (nested Markdown lists, indented code).
    let (view, cx) = open_body(cx, "");
    cx.simulate_input("- one\n  - two");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("- three");
    assert_eq!(body_text(&view, cx), "- one\n  - two\n  - three");
}

#[gpui::test]
fn undo_and_redo_in_the_body(cx: &mut TestAppContext) {
    let (view, cx) = open_body(cx, "Hello");
    view.update_in(cx, |c, _, cx| {
        c.body.update(cx, |b, cx| b.set_selected_range(5..5, cx));
    });
    cx.simulate_input(" world");
    assert_eq!(body_text(&view, cx), "Hello world");
    cx.simulate_keystrokes(UNDO);
    assert_eq!(body_text(&view, cx), "Hello");
    cx.simulate_keystrokes(REDO);
    assert_eq!(body_text(&view, cx), "Hello world");
}

#[gpui::test]
fn cut_copy_and_paste_in_the_body(cx: &mut TestAppContext) {
    let (view, cx) = open_body(cx, "Grüße 🎉");
    // gpui-kit binds editing keys per platform: cmd on macOS, ctrl elsewhere.
    cx.simulate_keystrokes("secondary-a secondary-c");
    assert_eq!(
        cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
        Some("Grüße 🎉")
    );
    cx.simulate_keystrokes("secondary-x");
    assert_eq!(body_text(&view, cx), "");
    cx.simulate_keystrokes("secondary-v secondary-v");
    assert_eq!(body_text(&view, cx), "Grüße 🎉Grüße 🎉");
    // The menu's actions do the same.
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("> quoted\n".into()));
    view.update_in(cx, |c, _, cx| {
        c.body.update(cx, |b, cx| b.set_selected_range(0..0, cx));
    });
    cx.dispatch_action(input::Paste);
    assert_eq!(body_text(&view, cx), "> quoted\nGrüße 🎉Grüße 🎉");
}

#[gpui::test]
fn the_context_menu_has_cut_copy_and_paste(cx: &mut TestAppContext) {
    let (view, cx) = open_body(cx, "Hallo Welt");
    let menu = |view: &Entity<Composer>, cx: &mut VisualTestContext| {
        view.read_with(cx, |c, cx| {
            body_menu_items(c.body.read(cx))
                .into_iter()
                .map(|item| item.map(|i| (i.label.to_string(), i.disabled, i.action.name())))
                .collect::<Vec<_>>()
        })
    };
    let item = |label: &str, disabled: bool, action: &dyn gpui::Action| {
        Some((label.to_owned(), disabled, action.name()))
    };
    // Nothing selected: nothing to cut or copy, no code-editor items.
    assert_eq!(
        menu(&view, cx),
        vec![
            item("Cut", true, &input::Cut),
            item("Copy", true, &input::Copy),
            item("Paste", false, &input::Paste),
            None,
            item("Select All", false, &input::SelectAll),
        ]
    );
    view.update_in(cx, |c, _, cx| {
        c.body.update(cx, |b, cx| b.set_selected_range(0..5, cx));
    });
    let items = menu(&view, cx);
    assert_eq!(items[0], item("Cut", false, &input::Cut));
    assert_eq!(items[1], item("Copy", false, &input::Copy));
}

#[gpui::test]
fn composing_text_with_an_input_method(cx: &mut TestAppContext) {
    use gpui::EntityInputHandler as _;
    let (view, cx) = open_body(cx, "Gr");
    view.update_in(cx, |c, window, cx| {
        c.body.update(cx, |b, cx| {
            b.set_selected_range(2..2, cx);
            // A dead key (macOS option-u) shows a marked "¨", the next key commits "ü".
            b.replace_and_mark_text_in_range(None, "¨", Some(1..1), window, cx);
            assert_eq!(b.value(), "Gr¨");
            assert_eq!(b.marked_text_range(window, cx), Some(2..3));
            b.replace_text_in_range(None, "ü", window, cx);
            assert_eq!(b.value(), "Grü");
            assert_eq!(b.marked_text_range(window, cx), None);
        });
    });
    cx.simulate_input("ße");
    assert_eq!(body_text(&view, cx), "Grüße");
    assert!(view.read_with(cx, |c, _| c.is_dirty()));

    view.update_in(cx, |c, window, cx| {
        c.body.update(cx, |b, cx| {
            // Japanese: the preedit changes as you type and is committed at once.
            b.replace_and_mark_text_in_range(None, " に", Some(2..2), window, cx);
            b.replace_and_mark_text_in_range(None, " にほ", Some(3..3), window, cx);
            b.replace_and_mark_text_in_range(None, " 日本", Some(3..3), window, cx);
            assert_eq!(b.value(), "Grüße 日本");
            b.replace_text_in_range(None, " 日本", window, cx);
            assert_eq!(b.marked_text_range(window, cx), None);
            // A cancelled preedit leaves nothing behind.
            b.replace_and_mark_text_in_range(None, "ka", Some(2..2), window, cx);
            b.replace_and_mark_text_in_range(None, "", None, window, cx);
            assert_eq!(b.value(), "Grüße 日本");
        });
    });
    draw(cx);
    // The committed composition is one undo step.
    cx.simulate_keystrokes(UNDO);
    assert_eq!(body_text(&view, cx), "Grüße");
    let draft = view.read_with(cx, |c, cx| c.draft(cx)).unwrap();
    assert_eq!(draft.body_markdown, "Grüße");
}

#[gpui::test]
fn long_lines_emoji_and_rtl_text_wrap_and_edit(cx: &mut TestAppContext) {
    let url = format!("https://example.com/{}?q=1", "very/long/path/".repeat(40));
    let paragraph = "A paragraph nobody breaks by hand, because it is prose. ".repeat(30);
    let text = format!(
        "{paragraph}\n{url}\nEmoji 🎉👍🏽 👨‍👩‍👧‍👦 🇦🇹 end\nRTL שלום עולם, مرحبا بالعالم (mixed).\n"
    );
    let (view, cx) = open_body(cx, &text);
    cx.simulate_resize(size(px(480.), px(360.)));
    draw(cx);
    let (rows, lines) = view.read_with(cx, |c, cx| {
        let b = c.body.read(cx);
        (b.display_map().wrap_row_count(), b.value().lines().count())
    });
    assert!(
        rows > lines + 10,
        "soft wrapped: {rows} rows for {lines} lines"
    );

    // Walk through every character and line, drawing as we go: no panics on
    // grapheme clusters, bidi text or wrapped rows.
    cx.simulate_keystrokes(TO_START);
    for _ in 0..text.chars().count() {
        cx.simulate_keystrokes("right");
    }
    draw(cx);
    assert_eq!(
        view.read_with(cx, |c, cx| c.body.read(cx).selected_range()),
        text.len()..text.len()
    );
    cx.simulate_keystrokes(TO_START);
    for _ in 0..rows {
        cx.simulate_keystrokes("down");
        draw(cx);
    }
    // Typing inside the RTL run and after the emoji.
    let rtl = text.find("עולם").unwrap();
    view.update_in(cx, |c, _, cx| {
        c.body
            .update(cx, |b, cx| b.set_selected_range(rtl..rtl, cx));
    });
    cx.simulate_input("x");
    let emoji_end = text.find(" end").unwrap();
    view.update_in(cx, |c, _, cx| {
        c.body
            .update(cx, |b, cx| b.set_selected_range(emoji_end..emoji_end, cx));
    });
    cx.simulate_input("!");
    draw(cx);
    let body = body_text(&view, cx);
    assert!(body.contains("שלום xעולם"), "{body}");
    assert!(body.contains("🇦🇹! end"), "{body}");
}

#[gpui::test]
fn typing_in_a_long_mail_stays_fast(cx: &mut TestAppContext) {
    // 2,000 lines, ~100 KB, a long paragraph every 100 lines; type in the middle and
    // draw a frame after every key. The release build measured ~0.55 ms per key and
    // frame with the real text system, the same as the plain text field; this only
    // guards against a gross regression (debug build, test text system).
    let mut text = String::new();
    for i in 0..2000 {
        if i % 100 == 99 {
            text.push_str(&"Ein langer Absatz über Größe, der weich umbrochen wird. ".repeat(12));
            text.push('\n');
        } else {
            text.push_str("Lorem ipsum dolor sit amet, consectetur elit.\n");
        }
    }
    assert!((100_000..130_000).contains(&text.len()), "{}", text.len());
    let (view, cx) = open_body(cx, &text);
    let mid = text.len() / 2;
    view.update_in(cx, |c, _, cx| {
        c.body
            .update(cx, |b, cx| b.set_selected_range(mid..mid, cx));
    });
    draw(cx);
    let keys = 200;
    let start = std::time::Instant::now();
    for _ in 0..keys {
        cx.simulate_input("a");
        draw(cx);
    }
    let per_key = start.elapsed() / keys;
    assert_eq!(body_text(&view, cx).len(), text.len() + keys as usize);
    assert!(
        per_key < Duration::from_millis(25),
        "{per_key:?} per key and frame"
    );
}

#[gpui::test]
fn composer_keys_reach_the_composer_from_the_body(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.to = vec![Address::new(None::<String>, "anna@example.com")];
    let (view, cx) = open(cx, backend.clone(), options);
    cx.simulate_input("Hi");
    let focused_body = |view: &Entity<Composer>, cx: &mut VisualTestContext| {
        view.update_in(cx, |c, window, cx| {
            c.body.read(cx).focus_handle(cx).is_focused(window)
        })
    };
    assert!(focused_body(&view, cx));

    // The code editor's own search panel stays off: cmd-f is the main window's.
    cx.simulate_keystrokes("cmd-f");
    cx.simulate_keystrokes("cmd-shift-f");
    assert!(!view.read_with(cx, |c, cx| c.body.read(cx).search_session().is_active()));

    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    assert_eq!(backend.saves().len(), 1);
    assert_eq!(backend.saves()[0].1.body_markdown, "Hi");

    cx.simulate_keystrokes("cmd-shift-a");
    assert!(cx.did_prompt_for_paths());

    cx.simulate_keystrokes("cmd-shift-c");
    assert!(view.read_with(cx, |c, _| c.cc_visible()));
    view.update_in(cx, |c, window, cx| {
        let focus = c.body.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    });

    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    let sends = backend.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].1.body_markdown, "Hi", "no line break added");
    assert!(view.read_with(cx, |c, _| c.is_closed()));
}

#[gpui::test]
fn tab_moves_from_the_subject_into_the_body(cx: &mut TestAppContext) {
    cx.update(|cx| {
        ui::init(cx);
        let _ = ui::keymap::bind_keymap(ui::keymap::DEFAULT_MACOS_KEYMAP, cx);
    });
    let mut composer = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let backend: Arc<dyn ComposeBackend> = Arc::new(FakeBackend::default());
        let view = cx.new(|cx| Composer::new(backend, options(), window, cx));
        composer = Some(view.clone());
        ui::kit::base::Root::new(view, window, cx)
    });
    let view = composer.unwrap();
    cx.run_until_parked();
    view.update_in(cx, |c, window, cx| {
        let focus = c.subject.focus_handle(cx);
        window.focus(&focus, cx);
    });
    draw(cx);
    cx.simulate_keystrokes("tab");
    cx.simulate_input("Body text");
    assert_eq!(body_text(&view, cx), "Body text");
    assert_eq!(
        view.read_with(cx, |c, cx| c.subject.read(cx).value().to_string()),
        ""
    );
}

// ---- spelling -----------------------------------------------------------------------

use ::spelling::Lang;

/// A composer with a recipient (the body has the focus) and `body`, spell checking in
/// `langs` (set before it opens, as the app does at startup).
fn open_spelling<'a>(
    cx: &'a mut TestAppContext,
    backend: Arc<FakeBackend>,
    langs: &[Lang],
    body: &str,
) -> (Entity<Composer>, &'a mut VisualTestContext) {
    let langs = langs.to_vec();
    cx.update(|cx| set_spellcheck(langs, cx));
    let mut options = options();
    options.draft.to = vec![Address::new(None::<String>, "anna@example.com")];
    options.draft.body_markdown = body.to_owned();
    open(cx, backend, options)
}

/// The underlined words, as the editor has them now.
fn marked(view: &Entity<Composer>, cx: &mut VisualTestContext) -> Vec<String> {
    view.read_with(cx, |c, cx| c.marked(cx))
        .into_iter()
        .map(|(_, word)| word)
        .collect()
}

/// Let the debounce timer fire and the background check finish.
fn after_pause(cx: &mut VisualTestContext) {
    cx.executor().advance_clock(SPELLCHECK_DEBOUNCE);
    cx.run_until_parked();
}

fn set_cursor(view: &Entity<Composer>, offset: usize, cx: &mut VisualTestContext) {
    view.update_in(cx, |c, _, cx| {
        c.body
            .update(cx, |b, cx| b.set_selected_range(offset..offset, cx));
    });
}

/// An entry of the body's right-click menu: label, disabled, action (`None`: separator).
type Entry = Option<(String, bool, Box<dyn gpui::Action>)>;

/// The body's right-click menu as it would open now.
fn body_menu(view: &Entity<Composer>, cx: &mut VisualTestContext) -> Vec<Entry> {
    view.update_in(cx, |c, _, cx| {
        c.body_menu(cx)
            .into_iter()
            .map(|item| item.map(|i| (i.label.to_string(), i.disabled, i.action)))
            .collect()
    })
}

fn labels(menu: &[Entry]) -> Vec<Option<&str>> {
    menu.iter()
        .map(|item| item.as_ref().map(|(label, _, _)| label.as_str()))
        .collect()
}

const PLAIN_MENU: [Option<&str>; 5] = [
    Some("Cut"),
    Some("Copy"),
    Some("Paste"),
    None,
    Some("Select All"),
];

#[gpui::test]
fn misspelled_words_are_underlined_after_a_pause(cx: &mut TestAppContext) {
    let (view, cx) = open_spelling(cx, Arc::default(), &[Lang::EnUs], "");
    cx.simulate_input("I will recieve it tomorrow");
    cx.run_until_parked();
    assert_eq!(marked(&view, cx), Vec::<String>::new(), "not while typing");
    after_pause(cx);
    assert_eq!(marked(&view, cx), ["recieve"]);
    assert_eq!(
        view.read_with(cx, |c, cx| c.marked(cx))[0].0,
        7..14,
        "byte range in the body"
    );
    assert_eq!(
        view.read_with(cx, |c, cx| c.spelling_status(cx)),
        Some("1 spelling issue · en-US".into())
    );

    // Fixing the word: the mark goes with the next check.
    view.update_in(cx, |c, _, cx| {
        c.body.update(cx, |b, cx| b.set_selected_range(7..14, cx));
    });
    cx.simulate_input("receive");
    after_pause(cx);
    assert_eq!(body_text(&view, cx), "I will receive it tomorrow");
    assert_eq!(marked(&view, cx), Vec::<String>::new());
    assert_eq!(
        view.read_with(cx, |c, cx| c.spelling_status(cx)),
        Some("✓ Spelling en-US".into())
    );
}

#[gpui::test]
fn a_draft_is_checked_when_it_opens_but_not_its_quotes_and_code(cx: &mut TestAppContext) {
    let body = "Thanks for teh update.\n\n> I qoute this line\n\n```\nlet wrod = 1;\n\nqiuck();\n```\n\nRun `cargo bulid` or see https://exmaple.com/pathh.";
    let (view, cx) = open_spelling(cx, Arc::default(), &[Lang::EnUs], body);
    assert_eq!(marked(&view, cx), ["teh"]);
}

#[gpui::test]
fn only_changed_paragraphs_are_checked_again(cx: &mut TestAppContext) {
    let body = "First paragrap here.\n\nSecond one is fine.\n\nThird wiht a typo.";
    let (view, cx) = open_spelling(cx, Arc::default(), &[Lang::EnUs], body);
    assert_eq!(marked(&view, cx), ["paragrap", "wiht"]);
    let blocks = view.read_with(cx, |c, _| c.spelling.cache_blocks());
    assert_eq!(blocks, 3);
    // Typing in the second paragraph shifts the third; its results are reused.
    set_cursor(&view, body.find(" one").unwrap(), cx);
    cx.simulate_input(" (taht)");
    after_pause(cx);
    assert_eq!(marked(&view, cx), ["paragrap", "taht", "wiht"]);
    let text = body_text(&view, cx);
    let ranges: Vec<Range<usize>> = view
        .read_with(cx, |c, cx| c.marked(cx))
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert_eq!(&text[ranges[2].clone()], "wiht");
}

#[gpui::test]
fn german_and_english_paragraphs_accept_each_others_words(cx: &mut TestAppContext) {
    let body = "Wir sehen uns morgen im Büro und besprechen dann alles weitere, tomorrow.\n\n\
        I will bring the Unterlagen and recieve the keys.\n\nLiebe Grüße";
    let (view, cx) = open_spelling(cx, Arc::default(), &[Lang::DeAt, Lang::EnUs], body);
    assert_eq!(marked(&view, cx), ["recieve"]);
    assert_eq!(
        view.read_with(cx, |c, cx| c.spelling_status(cx)),
        Some("1 spelling issue · de-AT · en-US".into())
    );
}

#[gpui::test]
fn right_clicking_a_misspelled_word_offers_suggestions(cx: &mut TestAppContext) {
    let (view, cx) = open_spelling(cx, Arc::default(), &[Lang::EnUs], "I hope to recieve it.");
    assert_eq!(marked(&view, cx), ["recieve"]);
    // Computed in the background before anyone asks.
    assert!(view.read_with(cx, |c, _| c.spelling.has_suggestions_for("recieve")));
    // Elsewhere: the plain menu.
    set_cursor(&view, 3, cx);
    assert_eq!(labels(&body_menu(&view, cx)), PLAIN_MENU);

    // On the word (gpui-kit put the cursor where the click was).
    set_cursor(&view, 12, cx);
    let menu = body_menu(&view, cx);
    let labels = labels(&menu);
    assert_eq!(labels[0], Some("receive"));
    let suggestions = labels.iter().take_while(|l| l.is_some()).count();
    assert!((1..=5).contains(&suggestions), "{labels:?}");
    assert_eq!(
        labels[suggestions..],
        [
            &[
                None,
                Some("Ignore Spelling"),
                Some("Add to Dictionary"),
                None
            ][..],
            &PLAIN_MENU[..]
        ]
        .concat()
    );

    // Choosing it replaces exactly that word.
    let (_, _, action) = menu[0].as_ref().unwrap();
    let action = action.boxed_clone();
    cx.update(|window, cx| window.dispatch_action(action, cx));
    assert_eq!(body_text(&view, cx), "I hope to receive it.");
    assert_eq!(marked(&view, cx), Vec::<String>::new(), "unmarked at once");
    after_pause(cx);
    assert_eq!(marked(&view, cx), Vec::<String>::new());
    // One undo step brings the typo back, and its mark with the next check.
    cx.simulate_keystrokes(UNDO);
    assert_eq!(body_text(&view, cx), "I hope to recieve it.");
    after_pause(cx);
    assert_eq!(marked(&view, cx), ["recieve"]);
}

#[gpui::test]
fn suggestions_keep_the_capitalization(cx: &mut TestAppContext) {
    let (view, cx) = open_spelling(cx, Arc::default(), &[Lang::EnUs], "Recieve it soon.");
    set_cursor(&view, 2, cx);
    let menu = body_menu(&view, cx);
    assert_eq!(labels(&menu)[0], Some("Receive"));
    let (_, _, action) = menu[0].as_ref().unwrap();
    let action = action.boxed_clone();
    cx.update(|window, cx| window.dispatch_action(action, cx));
    assert_eq!(body_text(&view, cx), "Receive it soon.");
}

#[gpui::test]
fn ignore_lasts_for_this_draft(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let body = "Greetings from Glasskube, and Glasskube again, with a typpo.";
    let (view, cx) = open_spelling(cx, backend.clone(), &[Lang::EnUs], body);
    assert_eq!(marked(&view, cx), ["Glasskube", "Glasskube", "typpo"]);
    cx.dispatch_action(IgnoreSpelling {
        word: "Glasskube".into(),
    });
    assert_eq!(marked(&view, cx), ["typpo"], "at once");
    cx.run_until_parked();
    assert_eq!(marked(&view, cx), ["typpo"]);
    cx.simulate_input(" Glasskube");
    after_pause(cx);
    assert_eq!(marked(&view, cx), ["typpo"]);
    assert!(backend.added_words().is_empty(), "nothing is kept");

    // Another draft still marks it.
    let other = cx.update(|window, cx| {
        let backend: Arc<dyn ComposeBackend> = backend.clone();
        let mut options = options();
        options.draft.body_markdown = "Hello Glasskube".into();
        cx.new(|cx| Composer::new(backend, options, window, cx))
    });
    cx.run_until_parked();
    assert_eq!(marked(&other, cx), ["Glasskube"]);
}

#[gpui::test]
fn add_to_dictionary_is_kept_for_every_draft(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend {
        spelling_words: vec!["Mazecon".into()],
        ..FakeBackend::default()
    });
    let body = "See you at Mazecon, says the Glasskube team.";
    let (view, cx) = open_spelling(cx, backend.clone(), &[Lang::EnUs], body);
    // Words from the stored dictionary are known from the start.
    assert_eq!(marked(&view, cx), ["Glasskube"]);
    set_cursor(&view, body.find("Glasskube").unwrap() + 3, cx);
    let menu = body_menu(&view, cx);
    let add = menu
        .iter()
        .flatten()
        .find(|(label, _, _)| label == "Add to Dictionary")
        .map(|(_, _, action)| action.boxed_clone())
        .unwrap();
    cx.update(|window, cx| window.dispatch_action(add, cx));
    assert_eq!(marked(&view, cx), Vec::<String>::new(), "at once");
    cx.run_until_parked();
    assert_eq!(backend.added_words(), ["Glasskube"]);
    assert_eq!(marked(&view, cx), Vec::<String>::new());

    // A draft opened later knows it too.
    let other = cx.update(|window, cx| {
        let backend: Arc<dyn ComposeBackend> = backend.clone();
        let mut options = options();
        options.draft.body_markdown = "The Glasskube and Mazecon folks, with a typpo.".into();
        cx.new(|cx| Composer::new(backend, options, window, cx))
    });
    cx.run_until_parked();
    assert_eq!(marked(&other, cx), ["typpo"]);
}

#[gpui::test]
fn with_spell_checking_off_nothing_is_marked(cx: &mut TestAppContext) {
    let body = "I will recieve it.";
    let (view, cx) = open_spelling(cx, Arc::default(), &[], body);
    assert_eq!(marked(&view, cx), Vec::<String>::new());
    assert_eq!(view.read_with(cx, |c, cx| c.spelling_status(cx)), None);
    set_cursor(&view, 9, cx);
    assert_eq!(labels(&body_menu(&view, cx)), PLAIN_MENU);

    // The setting changes while the draft is open.
    cx.update(|_, cx| set_spellcheck(vec![Lang::EnGb], cx));
    cx.run_until_parked();
    assert_eq!(marked(&view, cx), ["recieve"]);
    cx.update(|_, cx| set_spellcheck(Vec::new(), cx));
    cx.run_until_parked();
    assert_eq!(marked(&view, cx), Vec::<String>::new());
}

#[gpui::test]
fn the_status_item_switches_languages_for_this_draft(cx: &mut TestAppContext) {
    let body = "Wir treffen uns morgen zum Frühstück im Kaffeehaus am Ring.";
    let (view, cx) = open_spelling(cx, Arc::default(), &[Lang::EnUs], body);
    let status = |view: &Entity<Composer>, cx: &mut VisualTestContext| {
        view.read_with(cx, |c, cx| c.spelling_status(cx))
            .map(|s| s.to_string())
    };
    // English only: the German words are unknown.
    assert!(marked(&view, cx).len() > 3, "{:?}", marked(&view, cx));
    cx.dispatch_action(ToggleSpellingLanguage { lang: Lang::DeAt });
    cx.run_until_parked();
    assert_eq!(marked(&view, cx), Vec::<String>::new());
    assert_eq!(
        status(&view, cx).as_deref(),
        Some("✓ Spelling en-US · de-AT")
    );
    cx.dispatch_action(ToggleSpellingLanguage { lang: Lang::EnUs });
    cx.dispatch_action(ToggleSpellingLanguage { lang: Lang::DeAt });
    cx.run_until_parked();
    assert_eq!(status(&view, cx).as_deref(), Some("Spelling off"));
    cx.simulate_input(" Fruhstuck");
    after_pause(cx);
    assert_eq!(marked(&view, cx), Vec::<String>::new());
    // Only this draft: the setting stays.
    assert_eq!(
        cx.update(|_, cx| cx.global::<SpellcheckSettings>().languages.clone()),
        [Lang::EnUs]
    );
}

// ---- recipient chips ------------------------------------------------------------------

use crate::recipients::{Recipient, RecipientField, SUGGEST_DEBOUNCE};
use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, Point};

fn addr(name: Option<&str>, email: &str) -> Address {
    Address::new(name, email)
}

fn chips(field: &Entity<RecipientField>, cx: &mut VisualTestContext) -> Vec<Recipient> {
    field.read_with(cx, |f, _| f.recipients().to_vec())
}

fn emails(field: &Entity<RecipientField>, cx: &mut VisualTestContext) -> Vec<String> {
    chips(field, cx)
        .into_iter()
        .map(|r| match r {
            Recipient::Address(a) => a.email,
            Recipient::Invalid(text) => format!("invalid:{text}"),
        })
        .collect()
}

fn field_text(field: &Entity<RecipientField>, cx: &mut VisualTestContext) -> String {
    field.read_with(cx, |f, cx| f.text(cx).to_string())
}

fn to_field(view: &Entity<Composer>, cx: &mut VisualTestContext) -> Entity<RecipientField> {
    view.read_with(cx, |c, _| c.to.clone())
}

fn cc_field(view: &Entity<Composer>, cx: &mut VisualTestContext) -> Entity<RecipientField> {
    view.read_with(cx, |c, _| c.cc.clone())
}

fn bcc_field(view: &Entity<Composer>, cx: &mut VisualTestContext) -> Entity<RecipientField> {
    view.read_with(cx, |c, _| c.bcc.clone())
}

fn focus(field: &Entity<RecipientField>, cx: &mut VisualTestContext) {
    let field = field.clone();
    cx.update(|window, cx| {
        let handle = field.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
}

fn is_focused(field: &Entity<RecipientField>, cx: &mut VisualTestContext) -> bool {
    let field = field.clone();
    cx.update(|window, cx| field.read(cx).focus_handle(cx).is_focused(window))
}

#[gpui::test]
fn a_comma_semicolon_or_enter_makes_a_chip(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend, options());
    let to = to_field(&view, cx);
    cx.simulate_input("anna@example.com,");
    assert_eq!(emails(&to, cx), ["anna@example.com"]);
    assert_eq!(field_text(&to, cx), "");
    // A comma inside quotes doesn't split; the semicolon after the address does.
    cx.simulate_input(" \"Doe, John\" <john@example.com>;");
    assert_eq!(
        chips(&to, cx)[1],
        Recipient::Address(addr(Some("Doe, John"), "john@example.com"))
    );
    assert_eq!(chips(&to, cx)[1].label(), "Doe, John");
    cx.simulate_input("carl@example.com");
    assert_eq!(chips(&to, cx).len(), 2, "not before Enter");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        emails(&to, cx),
        ["anna@example.com", "john@example.com", "carl@example.com"]
    );
    // The same address twice is one chip.
    cx.simulate_input("Anna@Example.com,");
    assert_eq!(chips(&to, cx).len(), 3);
    let draft = view.read_with(cx, |c, cx| c.draft(cx)).unwrap();
    assert_eq!(draft.to.len(), 3);
    assert!(view.read_with(cx, |c, _| c.is_dirty()));
}

#[gpui::test]
fn leaving_the_field_makes_chips_and_keeps_the_focus_order(cx: &mut TestAppContext) {
    cx.update(|cx| {
        ui::init(cx);
        let _ = ui::keymap::bind_keymap(ui::keymap::DEFAULT_MACOS_KEYMAP, cx);
    });
    let mut composer = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let backend: Arc<dyn ComposeBackend> = Arc::new(FakeBackend::default());
        let mut options = options();
        options.draft.cc = vec![addr(None, "cc@example.com")];
        let view = cx.new(|cx| Composer::new(backend, options, window, cx));
        composer = Some(view.clone());
        ui::kit::base::Root::new(view, window, cx)
    });
    let view = composer.unwrap();
    cx.run_until_parked();
    draw(cx);
    let (to, cc, bcc) = (
        to_field(&view, cx),
        cc_field(&view, cx),
        bcc_field(&view, cx),
    );
    assert!(is_focused(&to, cx));
    cx.simulate_input("anna@example.com");
    // To → (the "Cc Bcc" button, as before) → Cc → Bcc → Subject → body, the typed
    // address a chip on the way.
    cx.simulate_keystrokes("tab");
    assert_eq!(emails(&to, cx), ["anna@example.com"]);
    assert!(!is_focused(&to, cx));
    cx.simulate_keystrokes("tab");
    assert!(is_focused(&cc, cx));
    cx.simulate_keystrokes("tab");
    assert!(is_focused(&bcc, cx));
    cx.simulate_input("nobody");
    cx.simulate_keystrokes("tab");
    // Text that isn't an address is a (red) chip too.
    assert_eq!(emails(&bcc, cx), ["invalid:nobody"]);
    assert!(view.update_in(cx, |c, window, cx| {
        c.subject.read(cx).focus_handle(cx).is_focused(window)
    }));
    cx.simulate_keystrokes("tab");
    assert!(view.update_in(cx, |c, window, cx| {
        c.body.read(cx).focus_handle(cx).is_focused(window)
    }));
    // Shift-tab goes back (from the subject: in the body it outdents).
    view.update_in(cx, |c, window, cx| {
        let focus = c.subject.focus_handle(cx);
        window.focus(&focus, cx);
    });
    cx.simulate_keystrokes("shift-tab");
    assert!(is_focused(&bcc, cx));
}

#[gpui::test]
fn pasting_a_list_makes_a_chip_per_address(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend.clone(), options());
    let to = to_field(&view, cx);
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
        "Anna <anna@example.com>, bob@example.org\nnobody".into(),
    ));
    cx.simulate_keystrokes("secondary-v");
    assert_eq!(
        chips(&to, cx),
        [
            Recipient::Address(addr(Some("Anna"), "anna@example.com")),
            Recipient::Address(addr(None, "bob@example.org")),
            Recipient::Invalid("nobody".into()),
        ]
    );
    assert_eq!(field_text(&to, cx), "");
    // The invalid chip stops sending with the message the plain field gave.
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    let error = view.read_with(cx, |c, _| c.error()).unwrap();
    assert_eq!(error, "“nobody” isn't an email address.");
    assert!(backend.sends().is_empty());

    // A single address pasted into the empty field is a chip too; pasting part of an
    // address into text is just text.
    cx.simulate_keystrokes("backspace backspace");
    assert_eq!(chips(&to, cx).len(), 2, "the invalid chip is gone");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("carl@example.com".into()));
    cx.simulate_keystrokes("secondary-v");
    assert_eq!(chips(&to, cx).len(), 3);
    cx.simulate_input("dora@");
    cx.write_to_clipboard(gpui::ClipboardItem::new_string("example.com".into()));
    cx.simulate_keystrokes("secondary-v");
    assert_eq!(field_text(&to, cx), "dora@example.com");
    cx.simulate_keystrokes("cmd-enter");
    cx.run_until_parked();
    let sends = backend.sends();
    assert_eq!(
        sends[0]
            .1
            .to
            .iter()
            .map(|a| a.email.as_str())
            .collect::<Vec<_>>(),
        [
            "anna@example.com",
            "bob@example.org",
            "carl@example.com",
            "dora@example.com"
        ]
    );
}

#[gpui::test]
fn suggestions_appear_after_a_pause_and_enter_takes_one(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend {
        contacts: vec![
            addr(Some("Anna Example"), "anna@example.com"),
            addr(Some("Andreas"), "andreas@example.org"),
            addr(Some("Bob"), "bob@example.org"),
        ],
        ..FakeBackend::default()
    });
    let (view, cx) = open(cx, backend, options());
    let to = to_field(&view, cx);
    let suggestions = |cx: &mut VisualTestContext| {
        to.read_with(cx, |f, _| {
            f.suggestions()
                .iter()
                .map(|a| a.email.clone())
                .collect::<Vec<_>>()
        })
    };
    cx.simulate_input("an");
    cx.run_until_parked();
    assert!(suggestions(cx).is_empty(), "not while typing");
    cx.executor().advance_clock(SUGGEST_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(suggestions(cx), ["anna@example.com", "andreas@example.org"]);
    draw(cx);
    assert!(
        cx.debug_bounds("to-suggestions").is_some(),
        "the list shows"
    );

    cx.simulate_keystrokes("down");
    assert_eq!(to.read_with(cx, |f, _| f.highlighted()), 1);
    cx.simulate_keystrokes("enter");
    assert_eq!(
        chips(&to, cx),
        [Recipient::Address(addr(
            Some("Andreas"),
            "andreas@example.org"
        ))]
    );
    assert_eq!(field_text(&to, cx), "");
    assert!(suggestions(cx).is_empty());

    // Escape closes the list, not the composer; addresses in the field already aren't
    // offered again; Tab takes the highlighted one.
    cx.simulate_input("a");
    cx.executor().advance_clock(SUGGEST_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(suggestions(cx), ["anna@example.com"]);
    cx.simulate_keystrokes("escape");
    assert!(suggestions(cx).is_empty());
    assert!(!view.read_with(cx, |c, _| c.is_closed()));
    cx.simulate_input("n");
    cx.executor().advance_clock(SUGGEST_DEBOUNCE);
    cx.run_until_parked();
    assert_eq!(suggestions(cx), ["anna@example.com"]);
    cx.simulate_keystrokes("tab");
    assert_eq!(emails(&to, cx), ["andreas@example.org", "anna@example.com"]);

    // A click takes one as well.
    cx.simulate_input("bo");
    cx.executor().advance_clock(SUGGEST_DEBOUNCE);
    cx.run_until_parked();
    draw(cx);
    let list = cx.debug_bounds("to-suggestions").unwrap();
    cx.simulate_click(list.center(), Modifiers::none());
    assert_eq!(
        emails(&to, cx),
        ["andreas@example.org", "anna@example.com", "bob@example.org"]
    );
    assert!(is_focused(&to, cx), "the focus stays in the field");
}

#[gpui::test]
fn backspace_selects_and_removes_the_last_chip(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.to = vec![
        addr(Some("Anna"), "anna@example.com"),
        addr(None, "bob@example.org"),
        addr(None, "carl@example.org"),
    ];
    let (view, cx) = open(cx, backend, options);
    let to = to_field(&view, cx);
    focus(&to, cx);
    cx.simulate_keystrokes("backspace");
    assert_eq!(to.read_with(cx, |f, _| f.selected()), Some(2));
    assert_eq!(chips(&to, cx).len(), 3, "selected first");
    cx.simulate_keystrokes("backspace");
    assert_eq!(emails(&to, cx), ["anna@example.com", "bob@example.org"]);
    assert!(view.read_with(cx, |c, _| c.is_dirty()));
    // Left walks through the chips, Delete removes the selected one.
    cx.simulate_keystrokes("left left");
    assert_eq!(to.read_with(cx, |f, _| f.selected()), Some(0));
    cx.simulate_keystrokes("delete");
    assert_eq!(emails(&to, cx), ["bob@example.org"]);
    // Typing ends the selection; Backspace then deletes text, not chips.
    cx.simulate_keystrokes("left");
    cx.simulate_input("x");
    assert_eq!(to.read_with(cx, |f, _| f.selected()), None);
    cx.simulate_keystrokes("backspace");
    assert_eq!(emails(&to, cx), ["bob@example.org"]);
    assert_eq!(field_text(&to, cx), "");
}

#[gpui::test]
fn clicking_selects_and_double_clicking_edits_a_chip(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let (view, cx) = open(cx, backend, options());
    let to = to_field(&view, cx);
    cx.simulate_input("anna@example.com, nobdy@example");
    cx.simulate_keystrokes("enter");
    cx.simulate_input("x");
    draw(cx);
    let chip = cx.debug_bounds("to-chip-1").unwrap();
    cx.simulate_click(chip.center(), Modifiers::none());
    assert_eq!(to.read_with(cx, |f, _| f.selected()), Some(1));
    assert!(is_focused(&to, cx));
    // The focus stayed in the field, so the typed "x" is still text.
    assert_eq!(field_text(&to, cx), "x");

    let double = |cx: &mut VisualTestContext, position: Point<gpui::Pixels>| {
        for click_count in [1, 2] {
            cx.simulate_event(MouseDownEvent {
                position,
                modifiers: Modifiers::none(),
                button: MouseButton::Left,
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                position,
                modifiers: Modifiers::none(),
                button: MouseButton::Left,
                click_count,
            });
        }
    };
    draw(cx);
    let chip = cx.debug_bounds("to-chip-1").unwrap();
    double(cx, chip.center());
    // The pending "x" went into a chip, the edited one into the text.
    assert_eq!(emails(&to, cx), ["anna@example.com", "invalid:x"]);
    assert_eq!(field_text(&to, cx), "nobdy@example");
    cx.simulate_input(".com");
    cx.simulate_keystrokes("enter");
    assert_eq!(
        emails(&to, cx),
        ["anna@example.com", "invalid:x", "nobdy@example.com"]
    );
}

#[gpui::test]
fn a_chip_can_be_dragged_from_to_to_a_collapsed_cc(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.to = vec![
        addr(Some("Anna"), "anna@example.com"),
        addr(Some("Bob"), "bob@example.org"),
    ];
    let (view, cx) = open(cx, backend, options);
    let (to, cc) = (to_field(&view, cx), cc_field(&view, cx));
    assert!(!view.read_with(cx, |c, _| c.cc_visible()));
    draw(cx);
    assert!(cx.debug_bounds("cc-field").is_none(), "collapsed");

    let start = cx.debug_bounds("to-chip-1").unwrap().center();
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(
        start + Point::new(gpui::px(10.), gpui::px(4.)),
        MouseButton::Left,
        Modifiers::none(),
    );
    draw(cx);
    // Dragging a chip shows Cc and Bcc as drop targets.
    let target = cx
        .debug_bounds("cc-field")
        .expect("Cc shows while dragging")
        .center();
    assert!(cx.debug_bounds("bcc-field").is_some());
    cx.simulate_mouse_move(target, MouseButton::Left, Modifiers::none());
    draw(cx);
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    draw(cx);

    assert_eq!(emails(&to, cx), ["anna@example.com"]);
    assert_eq!(
        chips(&cc, cx),
        [Recipient::Address(addr(Some("Bob"), "bob@example.org"))]
    );
    assert!(view.read_with(cx, |c, _| c.cc_visible()), "Cc stays open");
    let draft = view.read_with(cx, |c, cx| c.draft(cx)).unwrap();
    assert_eq!(draft.cc, vec![addr(Some("Bob"), "bob@example.org")]);
    assert!(view.read_with(cx, |c, _| c.is_dirty()));

    // And back, onto Anna's chip: in front of her.
    let start = cx.debug_bounds("cc-chip-0").unwrap().center();
    let target = cx.debug_bounds("to-chip-0").unwrap().center();
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(target, MouseButton::Left, Modifiers::none());
    draw(cx);
    cx.simulate_mouse_move(target, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(target, MouseButton::Left, Modifiers::none());
    assert_eq!(emails(&to, cx), ["bob@example.org", "anna@example.com"]);
    assert!(chips(&cc, cx).is_empty());

    // A drag let go elsewhere changes nothing and Cc and Bcc collapse again.
    view.update_in(cx, |c, window, cx| c.toggle_cc(window, cx));
    draw(cx);
    assert!(!view.read_with(cx, |c, _| c.cc_visible()));
    let start = cx.debug_bounds("to-chip-0").unwrap().center();
    let away = start + Point::new(gpui::px(0.), gpui::px(300.));
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_move(away, MouseButton::Left, Modifiers::none());
    draw(cx);
    cx.simulate_mouse_up(away, MouseButton::Left, Modifiers::none());
    draw(cx);
    assert_eq!(emails(&to, cx), ["bob@example.org", "anna@example.com"]);
    assert!(cx.debug_bounds("cc-field").is_none());
}

#[gpui::test]
fn drafts_and_reply_all_open_with_chips_and_save_the_same_lists(cx: &mut TestAppContext) {
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    // A reply-all as the core prefills it.
    options.draft.to = vec![addr(Some("Anna Example"), "anna@example.com")];
    options.draft.cc = vec![
        addr(Some("Bob"), "bob@example.net"),
        addr(None, "carol@example.com"),
    ];
    options.draft.bcc = vec![addr(Some("Example, The Boss"), "boss@example.org")];
    options.draft.origin = Some(DraftOrigin::Reply(MessageId(7)));
    options.draft.body_markdown = "\n\n> quoted".into();
    let expected = options.draft.clone();
    let (view, cx) = open(cx, backend.clone(), options);
    assert!(view.read_with(cx, |c, _| c.cc_visible()));
    let (to, cc, bcc) = (
        to_field(&view, cx),
        cc_field(&view, cx),
        bcc_field(&view, cx),
    );
    assert_eq!(
        chips(&to, cx)
            .iter()
            .map(Recipient::label)
            .collect::<Vec<_>>(),
        ["Anna Example"]
    );
    assert_eq!(
        chips(&cc, cx)
            .iter()
            .map(Recipient::label)
            .collect::<Vec<_>>(),
        ["Bob", "carol@example.com"]
    );
    assert_eq!(
        chips(&bcc, cx)[0].detail(),
        "\"Example, The Boss\" <boss@example.org>"
    );
    // The body has the focus in a reply.
    cx.simulate_input("Thanks!");
    cx.simulate_keystrokes("cmd-s");
    cx.run_until_parked();
    let (_, saved) = backend.saves().pop().unwrap();
    assert_eq!(saved.to, expected.to);
    assert_eq!(saved.cc, expected.cc);
    assert_eq!(saved.bcc, expected.bcc);

    // Opening the saved draft again: the same chips.
    let backend2: Arc<dyn ComposeBackend> = backend;
    let mut reopened = ComposerOptions::new(saved.clone());
    reopened.draft_id = Some(DraftId(42));
    let other = cx.update(|window, cx| cx.new(|cx| Composer::new(backend2, reopened, window, cx)));
    let draft = other.read_with(cx, |c, cx| c.draft(cx)).unwrap();
    assert_eq!(
        (draft.to, draft.cc, draft.bcc),
        (saved.to, saved.cc, saved.bcc)
    );
}

#[gpui::test]
fn a_draft_to_a_contact_opens_with_its_chip(cx: &mut TestAppContext) {
    // New Mail in the Address Book opens a draft with the contact in To.
    let backend = Arc::new(FakeBackend::default());
    let mut options = options();
    options.draft.to = vec![addr(Some("Anna Example"), "anna@example.com")];
    let (view, cx) = open(cx, backend, options);
    let to = to_field(&view, cx);
    assert_eq!(
        chips(&to, cx),
        [Recipient::Address(addr(
            Some("Anna Example"),
            "anna@example.com"
        ))]
    );
    assert_eq!(chips(&to, cx)[0].label(), "Anna Example");
    assert_eq!(field_text(&to, cx), "");
}

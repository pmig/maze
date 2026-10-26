//! Reader and original mode of HTML mail (ADR-020). The engine itself doesn't run in
//! these tests (it is a cargo feature, and headless); they check which mode each place
//! asks for, the switch, and the fallback of a build without the engine.

use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, TestAppContext};
use html_view::OriginalView;

use super::*;
use crate::message_view::{BodyMode, BodyModes, ViewPlace, set_body_modes};
use crate::source::MessageDocument;

fn open_tab_by_double_click(
    view: &Entity<MailWindow>,
    cx: &mut VisualTestContext,
) -> Entity<MessageTab> {
    let bounds = cx.debug_bounds("card-row-1").expect("a row");
    for click_count in [1, 2] {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: bounds.center(),
            modifiers: Modifiers::none(),
            click_count,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: bounds.center(),
            modifiers: Modifiers::none(),
            click_count,
        });
    }
    cx.run_until_parked();
    view.read_with(cx, |w, cx| w.active_message_tab(cx))
        .expect("a message tab is active")
}

#[gpui::test]
fn the_pane_reads_and_a_double_clicked_tab_shows_the_original(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let pane_view = view.read_with(cx, |w, cx| w.message_view(cx).unwrap());
    pane_view.read_with(cx, |v, cx| {
        assert_eq!(v.place(), ViewPlace::Pane);
        assert!(v.has_original(), "the fake's mail is HTML");
        assert_eq!(v.requested_mode(cx), BodyMode::Reader);
        assert_eq!(v.shown_mode(cx), BodyMode::Reader);
    });

    let tab = open_tab_by_double_click(&view, cx);
    let tab_view = tab.read_with(cx, |t, _| t.view().clone());
    tab_view.read_with(cx, |v, cx| {
        assert_eq!(v.place(), ViewPlace::Tab);
        assert_eq!(v.requested_mode(cx), BodyMode::Original);
        // Without the engine (the `servo` feature) the tab stays in reader mode.
        let expected = if OriginalView::AVAILABLE {
            BodyMode::Original
        } else {
            BodyMode::Reader
        };
        assert_eq!(v.shown_mode(cx), expected);
        assert_eq!(v.original_view().is_some(), OriginalView::AVAILABLE);
    });
    if !OriginalView::AVAILABLE {
        assert!(
            cx.debug_bounds("message-view").is_some(),
            "the tab still shows the message"
        );
    }
}

#[gpui::test]
fn cmd_alt_o_and_the_buttons_switch_the_mode(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let tab = open_tab_by_double_click(&view, cx);
    let tab_view = tab.read_with(cx, |t, _| t.view().clone());
    let mode = |cx: &mut VisualTestContext| tab_view.read_with(cx, |v, cx| v.requested_mode(cx));
    assert_eq!(mode(cx), BodyMode::Original);
    // The tab has the focus: the shortcut switches its message.
    cx.simulate_keystrokes("cmd-alt-o");
    assert_eq!(mode(cx), BodyMode::Reader);
    cx.simulate_keystrokes("cmd-alt-o");
    assert_eq!(mode(cx), BodyMode::Original);
    // The header buttons.
    let reader = cx.debug_bounds("mode-reader").expect("a Reader button");
    cx.simulate_click(reader.center(), Modifiers::none());
    assert_eq!(mode(cx), BodyMode::Reader);
    let original = cx
        .debug_bounds("mode-original")
        .expect("an Original button");
    cx.simulate_click(original.center(), Modifiers::none());
    assert_eq!(mode(cx), BodyMode::Original);

    // From the list, the shortcut switches the message next to it.
    cx.simulate_keystrokes("cmd-1");
    let pane_view = view.read_with(cx, |w, cx| w.message_view(cx).unwrap());
    view.update_in(cx, |w, window, cx| {
        let list = w.thread_list(cx).unwrap();
        window.focus(&list.focus_handle(cx), cx);
    });
    assert_eq!(
        pane_view.read_with(cx, |v, cx| v.requested_mode(cx)),
        BodyMode::Reader
    );
    cx.simulate_keystrokes("cmd-alt-o");
    assert_eq!(
        pane_view.read_with(cx, |v, cx| v.requested_mode(cx)),
        BodyMode::Original
    );
    // The next message starts in the default mode again.
    cx.simulate_keystrokes("f");
    assert_eq!(
        pane_view.read_with(cx, |v, cx| v.requested_mode(cx)),
        BodyMode::Reader
    );
}

#[gpui::test]
fn the_settings_pick_the_default_per_place(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let pane_view = view.read_with(cx, |w, cx| w.message_view(cx).unwrap());
    cx.update(|_, cx| {
        set_body_modes(
            BodyModes {
                pane: BodyMode::Original,
                tab: BodyMode::Reader,
            },
            cx,
        );
    });
    assert_eq!(
        pane_view.read_with(cx, |v, cx| v.requested_mode(cx)),
        BodyMode::Original
    );
    let tab = open_tab_by_double_click(&view, cx);
    let tab_view = tab.read_with(cx, |t, _| t.view().clone());
    assert_eq!(
        tab_view.read_with(cx, |v, cx| v.requested_mode(cx)),
        BodyMode::Reader
    );
}

#[test]
fn reader_settings_map_to_modes() {
    let defaults = BodyModes::from_settings(&settings::ReaderSettings::default());
    assert_eq!(defaults.for_place(ViewPlace::Pane), BodyMode::Reader);
    assert_eq!(defaults.for_place(ViewPlace::Tab), BodyMode::Original);
    let swapped = BodyModes::from_settings(&settings::ReaderSettings {
        default_mode: settings::ReaderMode::Original,
        tab_mode: settings::ReaderMode::PlainText,
        ..settings::ReaderSettings::default()
    });
    assert_eq!(swapped.pane, BodyMode::Original);
    // Plain text isn't a mode of its own yet: the reader.
    assert_eq!(swapped.tab, BodyMode::Reader);
}

/// The remote content banner of the original mode (`reader.remote_content = "ask"`).
#[gpui::test]
fn ask_shows_the_banner_until_load_images_or_always_for_this_sender(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let tab = open_tab_by_double_click(&view, cx);
    let tab_view = tab.read_with(cx, |t, _| t.view().clone());
    let with_remote = |doc: &mut MessageDocument, allowed: bool, ask: bool| {
        let original = doc.original.as_mut().unwrap();
        original.html = mazemail_types::Sensitive::new(std::sync::Arc::from(
            r#"<p>Sale</p><img src="https://cdn.shop.example/hero.png"><img src="cid:logo">
               <img src="https://cdn.shop.example/hero.png"><img src="https://cdn.shop.example/b.png">"#,
        ));
        original.remote_allowed = allowed;
        original.remote_ask = ask;
        original.trackers_removed = 2;
    };
    let banner = |cx: &mut VisualTestContext| tab_view.read_with(cx, |v, cx| v.remote_banner(cx));

    // Ask: blocked, two distinct remote images (the inline one is part of the message).
    tab_view.update(cx, |v, cx| {
        let mut doc = v.document().unwrap().clone();
        with_remote(&mut doc, false, true);
        v.show(Some(doc), cx);
    });
    let shown = banner(cx).expect("the banner shows");
    assert_eq!(shown.blocked, 2);
    assert_eq!(shown.trackers, 2);
    // Reader mode doesn't load remote images either: no banner there.
    tab_view.update(cx, |v, cx| v.set_mode(BodyMode::Reader, cx));
    assert!(banner(cx).is_none());
    tab_view.update(cx, |v, cx| v.set_mode(BodyMode::Original, cx));

    // "Load images": this message only.
    tab_view.update(cx, |v, cx| v.load_remote_once(cx));
    assert!(banner(cx).is_none());
    assert!(tab_view.read_with(cx, |v, _| v.remote_loaded_once()));
    // The next message asks again.
    tab_view.update(cx, |v, cx| {
        let mut doc = v.document().unwrap().clone();
        doc.message = MessageId(doc.message.0 + 100_000);
        with_remote(&mut doc, false, true);
        v.show(Some(doc), cx);
    });
    assert!(banner(cx).is_some());
    assert!(!tab_view.read_with(cx, |v, _| v.remote_loaded_once()));

    // "Never" and allowed senders: no banner.
    tab_view.update(cx, |v, cx| {
        let mut doc = v.document().unwrap().clone();
        with_remote(&mut doc, false, false);
        v.show(Some(doc), cx);
    });
    assert!(banner(cx).is_none(), "never: blocked without asking");
    tab_view.update(cx, |v, cx| {
        let mut doc = v.document().unwrap().clone();
        with_remote(&mut doc, true, true);
        v.show(Some(doc), cx);
    });
    assert!(banner(cx).is_none(), "allowed: nothing blocked");
}

#[gpui::test]
fn always_for_this_sender_is_kept_by_the_source(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.run_until_parked();
    let tab = open_tab_by_double_click(&view, cx);
    let tab_view = tab.read_with(cx, |t, _| t.view().clone());
    let allowed = |cx: &mut VisualTestContext| {
        tab_view.read_with(cx, |v, _| {
            v.document()
                .and_then(|doc| doc.original.as_ref())
                .is_some_and(|original| original.remote_allowed)
        })
    };
    assert!(!allowed(cx));
    tab_view.update(cx, |v, cx| v.allow_remote_for_sender(cx));
    // The fake keeps the sender and applies it when the message is read again.
    assert!(allowed(cx));
}

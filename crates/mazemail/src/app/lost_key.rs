//! The lost-key dialog (PLAN §4.17, "Lost key and recovery").
//!
//! Encrypted mail exists but the key store has no key. The dialog offers exactly the
//! two ways out the core provides ([`LostKey`]):
//!
//! - **Enter recovery key**: checked against the existing data before it is stored;
//!   a wrong key keeps the dialog open with an error.
//! - **Start fresh and re-sync**: after a confirmation, the unreadable data is moved
//!   aside (never deleted) and a new, empty store is created.
//!
//! The dialog can't be dismissed: without a key there is nothing to show.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    App, AppContext as _, Entity, ParentElement as _, SharedString, Styled as _, Window, div,
    prelude::FluentBuilder as _,
};
use mazemail_core::{CoreHandle, Error as CoreError, LostKey};
use mazemail_types::PRODUCT_NAME;
use ui::kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};

/// The dialog's state, shared by its buttons.
struct Recovery {
    /// `None` while an attempt runs.
    lost: Option<LostKey>,
    error: Option<SharedString>,
    busy: bool,
}

type Shared = Rc<RefCell<Recovery>>;

/// Opens the dialog in the main window.
pub(crate) fn show(lost: LostKey, cx: &mut App) {
    let Some(window) = super::main_window(cx) else {
        tracing::error!("the storage key is missing and no window is open to ask");
        return;
    };
    let state: Shared = Rc::new(RefCell::new(Recovery {
        lost: Some(lost),
        error: None,
        busy: false,
    }));
    let opened = window.update(cx, |_, window, cx| {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("XXXXX-XXXXX-XXXXX-…")
                .masked(true)
        });
        open(state, input, window, cx);
    });
    if opened.is_err() {
        tracing::error!("cannot show the recovery dialog");
    }
}

fn open(state: Shared, input: Entity<InputState>, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let theme = cx.theme();
        let (error, busy) = {
            let state = state.borrow();
            (state.error.clone(), state.busy)
        };
        let unlock = {
            let state = state.clone();
            let input = input.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                let key = input.read(cx).value().trim().to_owned();
                if key.is_empty() {
                    set_error(
                        &state,
                        "Enter the recovery key from Settings → Privacy.",
                        window,
                        cx,
                    );
                    return;
                }
                unlock(state.clone(), key, window, cx);
            }
        };
        let fresh = {
            let state = state.clone();
            move |_: &gpui::ClickEvent, window: &mut Window, cx: &mut App| {
                confirm_start_fresh(state.clone(), window, cx);
            }
        };
        dialog
            .title(format!("Unlock your mail in {PRODUCT_NAME}"))
            .keyboard(false)
            .close_button(false)
            .overlay_closable(false)
            .width(gpui::px(520.))
            .child(
                v_flex()
                    .gap_3()
                    .text_sm()
                    .child(
                        "Your mail on this computer is encrypted, but the key that opens it \
                         is missing from the key store (for example after restoring the data \
                         onto a new computer). Nothing has been deleted.",
                    )
                    .child(
                        "Enter the recovery key you saved when you set up Maze, or start \
                         fresh: Maze then moves the unreadable data aside and downloads your \
                         mail again.",
                    )
                    .child(Input::new(&input))
                    .when_some(error, |this, error| {
                        this.child(div().text_color(theme.danger).child(error))
                    }),
            )
            .footer(
                h_flex()
                    .w_full()
                    .justify_between()
                    .child(
                        Button::new("start-fresh")
                            .label("Start fresh and re-sync")
                            .small()
                            .disabled(busy)
                            .on_click(fresh),
                    )
                    .child(
                        Button::new("unlock")
                            .primary()
                            .label(if busy {
                                "Unlocking…"
                            } else {
                                "Enter recovery key"
                            })
                            .small()
                            .disabled(busy)
                            .on_click(unlock),
                    ),
            )
    });
}

fn set_error(state: &Shared, error: impl Into<SharedString>, window: &mut Window, cx: &mut App) {
    {
        let mut state = state.borrow_mut();
        state.error = Some(error.into());
        state.busy = false;
    }
    window.refresh();
    let _ = cx;
}

/// Tries the recovery key; on success the dialog closes and the mail opens.
fn unlock(state: Shared, key: String, window: &mut Window, cx: &mut App) {
    let Some(lost) = state.borrow_mut().lost.take() else {
        return;
    };
    {
        let mut state = state.borrow_mut();
        state.busy = true;
        state.error = None;
    }
    window.refresh();
    let attempt = cx.background_spawn(async move { lost.enter_recovery_key(&key).await });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = attempt.await;
        cx.update(|cx| match result {
            Ok(core) => opened(&core, cx),
            Err(failed) => {
                let message = match &failed.error {
                    CoreError::WrongRecoveryKey => {
                        "That recovery key doesn't open this mail. Check it and try again."
                            .to_owned()
                    }
                    other => format!("The key couldn't be used: {other}"),
                };
                match failed.lost_key {
                    Some(lost) => {
                        state.borrow_mut().lost = Some(*lost);
                        let _ = handle.update(cx, |_, window, cx| {
                            set_error(&state, message, window, cx);
                        });
                    }
                    // The key was stored but opening failed: the next start opens it.
                    None => {
                        let _ = handle.update(cx, |_, window, cx| window.close_dialog(cx));
                        super::fail(
                            format!("{message}. Quit and start {PRODUCT_NAME} again."),
                            cx,
                        );
                    }
                }
            }
        });
    })
    .detach();
}

/// Asks once more before moving the data aside.
fn confirm_start_fresh(state: Shared, window: &mut Window, cx: &mut App) {
    window.open_alert_dialog(cx, move |alert, _, _| {
        let state = state.clone();
        alert
            .title("Start fresh?")
            .description(
                "Maze moves the unreadable mail data into a folder named \
                 “mail-unreadable-…” next to it (you can delete it later, or restore it \
                 if the key turns up) and starts with an empty store. Mail on your servers \
                 is downloaded again.",
            )
            .ok_text("Start fresh")
            .cancel_text("Back")
            .show_cancel(true)
            .on_ok(move |_, window, cx| {
                start_fresh(&state, window, cx);
                true
            })
    });
}

fn start_fresh(state: &Shared, window: &mut Window, cx: &mut App) {
    let Some(lost) = state.borrow_mut().lost.take() else {
        return;
    };
    state.borrow_mut().busy = true;
    window.refresh();
    let attempt = cx.background_spawn(async move { lost.start_fresh().await });
    let handle = window.window_handle();
    cx.spawn(async move |cx| {
        let result = attempt.await;
        cx.update(|cx| match result {
            Ok((core, moved)) => {
                tracing::warn!(moved_to = %moved.display(), "started fresh; the old data was kept");
                opened(&core, cx);
            }
            Err(error) => {
                let _ = handle.update(cx, |_, window, cx| window.close_all_dialogs(cx));
                super::fail(format!("Starting fresh failed: {error}"), cx);
            }
        });
    })
    .detach();
}

/// The core is open: close the dialogs and show the mail.
fn opened(core: &CoreHandle, cx: &mut App) {
    if let Some(window) = super::main_window(cx) {
        let _ = window.update(cx, |_, window, cx| window.close_all_dialogs(cx));
    }
    super::core_ready(core, cx);
}

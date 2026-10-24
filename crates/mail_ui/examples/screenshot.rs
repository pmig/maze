//! Renders the Maze main window headlessly and saves it as a PNG.
//!
//! ```sh
//! cargo run -p mail_ui --example screenshot -- docs/screenshots/s1-main-window.png 1600 1000
//! ```
//!
//! Uses GPUI's headless app context with the Metal headless renderer: the window is
//! never shown, its size isn't limited by the screen, and no screen-recording
//! permission is needed. The image is at 2x scale. Set `MAZEMAIL_SCREENSHOT_DARK=1`
//! for the dark theme and `MAZEMAIL_SCREENSHOT_TABLE=1` for the table view.
//!
//! `MAZEMAIL_SCREENSHOT_LIST` sets up the message list instead:
//!
//! - `table-threaded`: table view with a branching thread expanded and one of its
//!   replies selected (`docs/screenshots/list-table-threaded.png`),
//! - `table-flat`: table view with threads off, Size and Account columns on,
//! - `cards-expanded`: cards with a branching thread expanded.

// A command-line tool: it reports where it saved the file.
#![allow(clippy::print_stdout)]

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, Context, HeadlessAppContext, px, size};
use mail_ui::thread_list::{ListKeys, ThreadList};
use mail_ui::ui_state::ColumnId;
use mail_ui::{FakeMailSource, MailSource, MailWindow, MailWindowOptions};
use ui::kit::component::{Theme, ThemeMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .unwrap_or_else(|| "docs/screenshots/s1-main-window.png".to_owned());
    let width: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1600.);
    let height: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1000.);
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");

    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(ui::assets::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        ui::init(cx);
        mail_ui::init(cx);
    });

    let source: Arc<dyn MailSource> = Arc::new(FakeMailSource::new());
    let source_for_setup = source.clone();
    let mut view = None;
    let window = cx.open_window(size(px(width), px(height)), |window, cx| {
        let mail = cx.new(|cx| MailWindow::new(source, &MailWindowOptions::default(), window, cx));
        view = Some(mail.clone());
        cx.new(|cx| ui::kit::base::Root::new(mail, window, cx))
    })?;
    if flag("MAZEMAIL_SCREENSHOT_TABLE")
        && let Some(view) = &view
    {
        cx.update_window(window.into(), |_, window, cx| {
            view.update(cx, |mail, cx| {
                mail.thread_list(cx)
                    .expect("a mail view")
                    .update(cx, |list, cx| list.toggle_mode(window, cx))
            });
        })?;
    }
    if let (Ok(list_setup), Some(view)) = (std::env::var("MAZEMAIL_SCREENSHOT_LIST"), &view) {
        let source = source_for_setup.clone();
        cx.update_window(window.into(), |_, window, cx| {
            view.update(cx, |mail, cx| {
                mail.thread_list(cx)
                    .expect("a mail view")
                    .update(cx, |list, cx| match list_setup.as_str() {
                        "table-threaded" => {
                            list.toggle_mode(window, cx);
                            expand_branching_thread(list, &source, cx);
                        }
                        "table-flat" => {
                            list.toggle_mode(window, cx);
                            list.toggle_threads(cx);
                            list.toggle_column(ColumnId::Account, cx);
                            list.toggle_column(ColumnId::Size, cx);
                            list.step(3, cx);
                        }
                        "cards-expanded" => expand_branching_thread(list, &source, cx),
                        other => println!("unknown MAZEMAIL_SCREENSHOT_LIST {other}"),
                    });
            });
        })?;
    }
    // After the window exists: it reports the platform's appearance when it opens.
    let mode = if flag("MAZEMAIL_SCREENSHOT_DARK") {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    cx.update(|cx| Theme::change(mode, None, cx));
    for _ in 0..4 {
        cx.advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            let arena = window.draw(cx);
            arena.clear(cx);
        })?;
    }
    let image = cx.capture_screenshot(window.into())?;
    image.save(&path)?;
    println!("saved {path} ({}x{})", image.width(), image.height());
    Ok(())
}

/// Expands the first thread near the top whose replies branch, and selects a reply.
fn expand_branching_thread(
    list: &mut ThreadList,
    source: &Arc<dyn MailSource>,
    cx: &mut Context<'_, ThreadList>,
) {
    let ListKeys::Threads(keys) = list.model().borrow().keys().clone() else {
        return;
    };
    let branching = keys.iter().take(40).position(|thread| {
        source.thread_tree(*thread).is_ok_and(|tree| {
            tree.iter().filter(|n| n.depth == 1).count() >= 2 && tree.iter().any(|n| n.depth >= 2)
        })
    });
    let Some(base) = branching else {
        return;
    };
    let row = list.model().borrow().expansions().visible_of(base);
    list.toggle_expanded(row, cx);
    list.select(row + 3, cx);
    // Show the thread near the top of the list.
    let above = row.saturating_sub(2);
    if list.mode() == mail_ui::ListMode::Cards {
        let slot = list.model().borrow().expansions().slot_of(above, 2);
        list.scroll_handle()
            .scroll_to_item_strict(slot, gpui::ScrollStrategy::Top);
    }
}

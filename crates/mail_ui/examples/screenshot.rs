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

// A command-line tool: it reports where it saved the file.
#![allow(clippy::print_stdout)]

use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, HeadlessAppContext, px, size};
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
                mail.thread_list()
                    .update(cx, |list, cx| list.toggle_mode(window, cx))
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

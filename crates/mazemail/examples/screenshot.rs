//! Renders the Maze main window on real data headlessly and saves it as a PNG.
//!
//! ```sh
//! cargo xtask seed-demo --data-dir /tmp/maze-demo
//! cargo run -p mazemail --example screenshot -- /tmp/maze-demo docs/screenshots/app-real-data.png 1600 1000
//! ```
//!
//! Opens the core on the data directory with the development key file
//! (`MAZEMAIL_INSECURE_DEV_KEYSTORE`'s store, as `seed-demo` creates it), shows it
//! through `CoreMailSource` in GPUI's headless app context with the Metal headless
//! renderer (no window on screen, no screen-recording permission), and captures it at
//! 2x scale. `MAZEMAIL_SCREENSHOT_DARK=1` uses the dark theme and
//! `MAZEMAIL_SCREENSHOT_FONT_SIZE=18` another `font_size`. The message list uses
//! the data directory's `ui_state.json` (per-folder view, threads, columns), as the
//! app does. Maze must not be running on the same data directory.

// A command-line tool: it reports where it saved the file.
#![allow(clippy::print_stdout)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{AppContext as _, HeadlessAppContext, px, size};
use mail_ui::{
    ListDefaults, MailSource, MailWindow, MailWindowOptions, UI_STATE_FILE, UiStateStore,
};
use mazemail::keys::insecure_dev_key_store;
use mazemail::source::CoreMailSource;
use mazemail_core::{CoreConfig, CoreHandle};
use ui::kit::component::{Theme, ThemeMode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let data_dir = PathBuf::from(
        args.next()
            .ok_or("usage: screenshot <data-dir> [out.png] [w] [h]")?,
    );
    let path = args
        .next()
        .unwrap_or_else(|| "docs/screenshots/app-real-data.png".to_owned());
    let width: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1600.);
    let height: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(1000.);

    let runtime = tokio::runtime::Runtime::new()?;
    let config = CoreConfig::new(&data_dir, insecure_dev_key_store(&data_dir))
        .with_runtime(runtime.handle().clone());
    let core = runtime.block_on(CoreHandle::open(config))?;

    let text_system = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text_system,
        Arc::new(ui::assets::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        ui::init(cx);
        ui::gpui_tokio::init_from_handle(cx, core.runtime());
        mail_ui::init(cx);
    });

    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let source: Arc<dyn MailSource> = Arc::new(CoreMailSource::new(core.clone()));
    let options = MailWindowOptions {
        ui_state: UiStateStore::load(data_dir.join(UI_STATE_FILE), ListDefaults::default()),
        local_offset,
        ..MailWindowOptions::default()
    };
    let window = cx.open_window(size(px(width), px(height)), |window, cx| {
        let mail = cx.new(|cx| MailWindow::new(source, &options, window, cx));
        cx.new(|cx| ui::kit::base::Root::new(mail, window, cx))
    })?;
    let mode = if std::env::var("MAZEMAIL_SCREENSHOT_DARK").is_ok_and(|v| v == "1") {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    cx.update(|cx| Theme::change(mode, None, cx));
    if let Some(font_size) = std::env::var("MAZEMAIL_SCREENSHOT_FONT_SIZE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        cx.update(|cx| {
            ui::theme::apply_choice(
                ui::theme::ThemeChoice {
                    appearance: match mode {
                        ThemeMode::Dark => ui::theme::Appearance::Dark,
                        ThemeMode::Light => ui::theme::Appearance::Light,
                    },
                    light: "One Light".to_owned(),
                    dark: "One Dark".to_owned(),
                    themes_dir: None,
                    font_size,
                },
                cx,
            );
        });
    }
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
    drop(cx);
    runtime.block_on(core.shutdown())?;
    Ok(())
}

//! The GUI: opens the Maze window on the GPUI platform.

use std::sync::Arc;
use std::time::Instant;

use mail_ui::perf::ScrollBenchmark;
use mail_ui::{FakeMailSource, MailSource, MailWindowOptions};
use time::UtcOffset;

/// What the GUI needs from `main`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AppOptions {
    pub(crate) process_start: Instant,
    pub(crate) local_offset: UtcOffset,
}

/// Runs the app until the user quits.
pub(crate) fn run(options: AppOptions) -> anyhow::Result<()> {
    // Until the core exists, the window shows synthetic mail.
    let source: Arc<dyn MailSource> = Arc::new(FakeMailSource::new());
    let window_options = MailWindowOptions {
        local_offset: options.local_offset,
        process_start: options.process_start,
        benchmark: ScrollBenchmark::from_env(),
        window_size: None,
    };

    let app = ui::kit::application().with_assets(ui::assets::Assets);
    // macOS keeps apps running without windows; clicking the Dock icon brings the
    // main window back.
    app.on_reopen({
        let source = source.clone();
        move |cx| {
            if cx.windows().is_empty() {
                open_window(&source, window_options, cx);
            }
        }
    });
    app.run(move |cx| {
        ui::init(cx);
        if let Err(error) = ui::gpui_tokio::init(cx) {
            tracing::error!(%error, "could not start the Tokio runtime");
        }
        mail_ui::init(cx);
        open_window(&source, window_options, cx);
        cx.activate(true);
        tracing::debug!(
            elapsed_ms = options.process_start.elapsed().as_secs_f64() * 1e3,
            "window opened"
        );
    });
    Ok(())
}

fn open_window(source: &Arc<dyn MailSource>, options: MailWindowOptions, cx: &mut gpui::App) {
    if let Err(error) = mail_ui::open_main_window(source.clone(), options, cx) {
        tracing::error!(%error, "could not open the main window");
        cx.quit();
    }
}

//! Scroll benchmark over the 1,000,000-thread folder, rendered headlessly.
//!
//! ```sh
//! cargo run --release -p mail_ui --example scroll_bench -- [frames] [px-per-frame] [cards|table] [last-frame.png]
//! ```
//!
//! Each frame moves the list by a fixed distance and draws the whole window (layout,
//! prepaint, paint: the main-thread work of a frame). Frames are timed from the start
//! of the draw to its end. The window is 1600×1000 at 2x scale, as on a Retina Mac.
//! A second pass starts 900,000 rows down, and a few frames are also rasterized with
//! the Metal headless renderer (including the read-back, so an upper bound for the GPU
//! part).
//!
//! `MAZEMAIL_BENCH_EXPAND=1` expands every thread first (`*`, reply trees fetched as
//! they scroll into view), `MAZEMAIL_BENCH_FLAT=1` turns threads off (one row per
//! message, about 1.9 M rows). The time of the folder switch, of "expand all" and of
//! turning threads off is printed too.
//!
//! The in-app benchmark (`MAZEMAIL_BENCH_SCROLL`) measures the same on a real display
//! including presentation; this one runs anywhere, display or not.

// A command-line tool: it prints its results.
#![allow(clippy::print_stdout)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{AnyWindowHandle, AppContext as _, Entity, HeadlessAppContext, point, px, size};
use mail_ui::perf::{Percentiles, resident_memory};
use mail_ui::source::ThreadScope;
use mail_ui::thread_list::{CARD_HEIGHT, ListMode, TABLE_ROW_HEIGHT};
use mail_ui::{FakeMailSource, MailSource, MailWindow, MailWindowOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let frames: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(1200);
    let speed: f32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(40.);
    let table = args.next().is_some_and(|v| v == "table");
    let last_frame_png = args.next();
    let flag = |name: &str| std::env::var(name).is_ok_and(|v| v == "1");
    let (expand, flat) = (flag("MAZEMAIL_BENCH_EXPAND"), flag("MAZEMAIL_BENCH_FLAT"));

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
    let mut view: Option<Entity<MailWindow>> = None;
    let handle = cx.open_window(size(px(1600.), px(1000.)), |window, cx| {
        let mail = cx.new(|cx| MailWindow::new(source, &MailWindowOptions::default(), window, cx));
        view = Some(mail.clone());
        cx.new(|cx| ui::kit::base::Root::new(mail, window, cx))
    })?;
    let window: AnyWindowHandle = handle.into();
    let view = view.ok_or("window view missing")?;

    cx.update_window(window, |_, window, cx| {
        view.update(cx, |mail, cx| {
            mail.thread_list(cx)
                .expect("a mail view")
                .update(cx, |list, cx| {
                    let started = Instant::now();
                    list.open(
                        ThreadScope::Folder(FakeMailSource::large_folder()),
                        "Archives".into(),
                        None,
                        cx,
                    );
                    println!(
                        "folder switch to 1M threads (keys + first rows): {:.1} ms",
                        started.elapsed().as_secs_f64() * 1e3
                    );
                    if table {
                        list.toggle_mode(window, cx);
                    }
                    if flat {
                        let started = Instant::now();
                        list.toggle_threads(cx);
                        println!(
                            "threads off: {} message keys in {:.1} ms",
                            list.key_count(),
                            started.elapsed().as_secs_f64() * 1e3
                        );
                    }
                });
        });
    })?;
    if expand {
        let started = Instant::now();
        cx.update_window(window, |_, _, cx| {
            view.update(cx, |mail, cx| {
                mail.thread_list(cx)
                    .expect("a mail view")
                    .update(cx, |list, cx| list.expand_all(cx));
            });
        })?;
        let on_main = started.elapsed();
        cx.run_until_parked();
        let rows = cx.update_window(window, |_, _, cx| {
            view.read(cx)
                .thread_list(cx)
                .expect("a mail view")
                .read(cx)
                .len()
        })?;
        println!(
            "expand all: {:.1} ms on the main thread, {:.1} ms until the counts are in, {rows} rows",
            on_main.as_secs_f64() * 1e3,
            started.elapsed().as_secs_f64() * 1e3,
        );
    }
    let row_height = if table { TABLE_ROW_HEIGHT } else { CARD_HEIGHT };

    let draw = |cx: &mut HeadlessAppContext,
                offset: f32|
     -> Result<Duration, Box<dyn std::error::Error>> {
        cx.update_window(window, |_, window, cx| {
            let list = view.read(cx).thread_list(cx).expect("a mail view").read(cx);
            let y = point(px(0.), px(-offset));
            match list.mode() {
                ListMode::Cards => list.scroll_handle().0.borrow().base_handle.set_offset(y),
                ListMode::Table => list
                    .table()
                    .read(cx)
                    .vertical_scroll_handle
                    .0
                    .borrow()
                    .base_handle
                    .set_offset(y),
            }
            window.refresh();
            let started = Instant::now();
            let arena = window.draw(cx);
            arena.clear(cx);
            started.elapsed()
        })
        .map_err(Into::into)
    };

    // Warm up: first frames load fonts and fill caches.
    for frame in 0..30 {
        draw(&mut cx, frame as f32 * speed)?;
    }

    let report = |label: &str,
                  start_row: f32,
                  cx: &mut HeadlessAppContext|
     -> Result<(), Box<dyn std::error::Error>> {
        let start = start_row * row_height;
        let mut samples = Vec::with_capacity(frames);
        for frame in 0..frames {
            samples.push(draw(cx, start + frame as f32 * speed)?);
        }
        let p = Percentiles::of(&samples).ok_or("no samples")?;
        let over_8ms = samples.iter().filter(|d| d.as_secs_f64() > 0.008).count();
        println!(
            "{label}: {} frames, {:.0} rows, cpu p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms, max {:.2} ms, frames over 8 ms: {over_8ms}",
            p.count,
            frames as f32 * speed / row_height,
            p.p50,
            p.p95,
            p.p99,
            p.max
        );
        Ok(())
    };
    let view_name = match (table, expand, flat) {
        (true, true, _) => "table 24px, all expanded",
        (true, _, true) => "table 24px, threads off",
        (true, _, _) => "table 24px",
        (false, true, _) => "cards 52px, all expanded",
        (false, _, true) => "cards 52px, threads off",
        (false, _, _) => "cards 52px",
    };
    report(&format!("{view_name}, top of 1M"), 0., &mut cx)?;
    report(&format!("{view_name}, from row 900k"), 900_000., &mut cx)?;

    let mut raster = Vec::new();
    let mut last = None;
    for _ in 0..20 {
        let started = Instant::now();
        last = Some(cx.capture_screenshot(window)?);
        raster.push(started.elapsed());
    }
    if let (Some(path), Some(image)) = (last_frame_png, last) {
        image.save(&path)?;
        println!("saved the last frame to {path}");
    }
    if let Some(p) = Percentiles::of(&raster) {
        println!(
            "rasterize + read back 3200x2000: p50 {:.2} ms, p95 {:.2} ms",
            p.p50, p.p95
        );
    }
    if let Some(rss) = resident_memory() {
        println!(
            "rss after benchmark: {:.0} MB",
            rss as f64 / (1024. * 1024.)
        );
    }
    Ok(())
}

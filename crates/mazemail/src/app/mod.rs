//! The GUI: the Maze window on real data (PLAN §4.9, §4.17 startup).
//!
//! 1. The window opens at once over a [`CoreMailSource`] that is still opening
//!    ("Opening Maze…"), while the core reads the key and opens the store in the
//!    background.
//! 2. When the core is open: the Tokio bridge (`ui::gpui_tokio`) switches to the core's
//!    runtime, the source shows the mail, the core's change events flow into the
//!    window ([`MailWindow::apply_changes`]), and the local API server starts on the
//!    core's runtime with the [GPUI approver](approval) (or the debug-only test
//!    auto-approver).
//! 3. Startup problems: another Maze on the data directory gets a clear message and
//!    quits; a missing key opens the [lost-key dialog](lost_key); anything else shows
//!    the error in the window.
//! 4. On quit the server stops (removing the socket) and the core shuts down cleanly.
//! 5. The [auto-updater](updates) starts with the window (before the core is open),
//!    follows the core's settings once it is, and the crash guard is disarmed after
//!    the window painted with the core open.
//! 6. New messages, replies and forwards open [draft tabs](compose) in the main window;
//!    the window's tabs and splits are saved to `ui_state.json` and restored (ADR-018),
//!    draft tabs once the core is open.
//! 7. "Settings…" (`cmd-,`) edits `settings.json` in a tab of the main window; saving
//!    reloads the core's settings at once ([`CoreHandle::reload_settings`]).
//! 8. Local Folders of an older Maze that still hold mail get a notice at every start
//!    ([`legacy_local`], ADR-019).
//! 9. New mail becomes system notifications ([`new_mail`], ADR-021); a click opens the
//!    message.
//!
//! `MAZEMAIL_DEMO=1` runs the window on synthetic mail (`FakeMailSource`) without a
//! core, a key or a socket.

mod add_account;
mod agents;
mod approval;
mod compose;
mod keyboard;
mod legacy_local;
mod lost_key;
mod new_mail;
mod reading;
mod theme;
mod updates;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use gpui::{AnyWindowHandle, App, AppContext as _, Entity, Global, PromptLevel};
use local_api::LocalApiServer;
use mail_ui::perf::ScrollBenchmark;
use mail_ui::{
    DataChanges, FakeMailSource, MailSource, MailWindow, MailWindowOptions, SettingsFile,
};
use mazemail::source::CoreMailSource;
use mazemail::{approve, keys, startup};
use mazemail_core::api::{self, TokenAuthorizer};
use mazemail_core::{CoreEventBatch, CoreHandle, Error as CoreError};
use mazemail_types::PRODUCT_NAME;
use settings::Paths;
use time::UtcOffset;
use tokio::sync::{broadcast, oneshot};

/// Set to `1` to show synthetic mail instead of the real data.
pub(crate) const DEMO_ENV: &str = "MAZEMAIL_DEMO";

/// How long quitting waits for the server and the core to stop.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// What the GUI needs from `main`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AppOptions {
    pub(crate) process_start: Instant,
    pub(crate) local_offset: UtcOffset,
}

/// The app's state, a GPUI global.
struct AppState {
    source: Arc<CoreMailSource>,
    authorizer: TokenAuthorizer,
    window_options: MailWindowOptions,
    window: Option<(AnyWindowHandle, Entity<MailWindow>)>,
    core: Option<CoreHandle>,
    server: Option<RunningServer>,
}

impl Global for AppState {}

/// The local API server task and the switch that stops it.
struct RunningServer {
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

/// Runs the app until the user quits.
pub(crate) fn run(options: AppOptions) -> anyhow::Result<()> {
    let mut window_options = MailWindowOptions {
        local_offset: options.local_offset,
        process_start: options.process_start,
        benchmark: ScrollBenchmark::from_env(),
        window_size: None,
        ..MailWindowOptions::default()
    };
    if std::env::var(DEMO_ENV).is_ok_and(|value| value.trim() == "1") {
        run_demo(window_options);
        return Ok(());
    }

    let paths = Paths::current().context("finding the data directory")?;
    window_options.ui_state = startup::ui_state(&paths);
    // Saving in the settings tab applies the file at once. Before the core is open
    // there is nothing to reload: it reads the file when it opens.
    window_options.settings_file = Some(SettingsFile::new(paths.settings_file()).on_save(
        |cx: &mut App| {
            if let Some(core) = cx.try_global::<AppState>().and_then(|s| s.core.clone()) {
                core.reload_settings();
            }
        },
    ));
    let key_store = keys::key_store(paths.data_dir())?;
    let secrets = keys::secret_store(paths.data_dir());
    let authorizer = TokenAuthorizer::new();
    let config = startup::core_config(
        &paths,
        key_store,
        secrets,
        &authorizer,
        options.local_offset,
    );
    let source = Arc::new(CoreMailSource::opening());

    let app = ui::kit::application().with_assets(ui::assets::Assets);
    // macOS keeps apps running without windows; clicking the Dock icon brings the
    // main window back.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_window(cx);
        }
    });
    let theme_paths = paths.clone();
    app.run(move |cx| {
        ui::init(cx);
        theme::apply_from_file(&theme_paths, cx);
        reading::apply_from_file(&theme_paths, cx);
        keyboard::apply_from_file(&theme_paths, cx);
        mail_ui::init(cx);
        composer::init(cx);
        auto_update_ui::init(cx);
        let mut menus = mail_ui::app_menus();
        auto_update_ui::add_menu_item(&mut menus);
        cx.set_menus(menus);
        cx.on_action(|_: &mail_ui::actions::AddAccount, cx| {
            // The action usually comes from a button or the palette, i.e. while the main
            // window is busy handling that event, and GPUI can't update a window from
            // inside its own event handling. Open the dialog once the event is done.
            cx.defer(|cx| match cx.global::<AppState>().core.clone() {
                Some(core) => add_account::show(core, cx),
                None => tracing::info!("Add Account: the mail data isn't open yet"),
            });
        });
        // The main window asks for new drafts itself (a `ComposeRequest`); this is
        // cmd-n from another window, e.g. a draft in a window of its own. Deferred, like
        // Add Account, because the window is busy handling that event.
        cx.on_action(|_: &mail_ui::actions::NewMessage, cx| cx.defer(compose::new_message));
        // Settings from another window (a draft in a window of its own): they open in
        // the main window, which handles the action itself when it has the focus.
        cx.on_action(|_: &mail_ui::actions::OpenSettings, cx| cx.defer(open_settings));
        agents::init(cx);
        cx.set_global(AppState {
            source,
            authorizer,
            window_options,
            window: None,
            core: None,
            server: None,
        });
        open_window(cx);
        cx.activate(true);
        updates::start(&paths, cx);
        tracing::debug!(
            elapsed_ms = options.process_start.elapsed().as_secs_f64() * 1e3,
            "window opened"
        );

        // The key is read and the store opened in the background (PLAN §4.17).
        let opening = cx.background_spawn(CoreHandle::open(config));
        cx.spawn(async move |cx| {
            let result = opening.await;
            cx.update(|cx| core_opened(result, cx));
        })
        .detach();

        cx.on_app_quit(|cx| {
            shutdown(cx);
            async {}
        })
        .detach();
    });
    Ok(())
}

fn run_demo(window_options: MailWindowOptions) {
    tracing::info!("{DEMO_ENV}=1: showing synthetic mail");
    let source: Arc<dyn MailSource> = Arc::new(FakeMailSource::new());
    let app = ui::kit::application().with_assets(ui::assets::Assets);
    app.on_reopen({
        let source = source.clone();
        let window_options = window_options.clone();
        move |cx| {
            if cx.windows().is_empty()
                && let Err(error) =
                    mail_ui::open_main_window(source.clone(), window_options.clone(), cx)
            {
                tracing::error!(%error, "could not open the main window");
            }
        }
    });
    app.run(move |cx| {
        ui::init(cx);
        if let Err(error) = ui::gpui_tokio::init(cx) {
            tracing::error!(%error, "could not start the Tokio runtime");
        }
        keyboard::apply_platform(cx);
        mail_ui::init(cx);
        if let Err(error) = mail_ui::open_main_window(source, window_options, cx) {
            tracing::error!(%error, "could not open the main window");
            cx.quit();
        }
        cx.activate(true);
    });
}

/// Opens the main window over the app's source and remembers it.
fn open_window(cx: &mut App) {
    let (source, options) = {
        let state = cx.global::<AppState>();
        (
            state.source.clone() as Arc<dyn MailSource>,
            state.window_options.clone(),
        )
    };
    match mail_ui::open_main_window(source, options, cx) {
        Ok(window) => {
            updates::add_button(&window.1, cx);
            // New Message, Reply, Reply All, Forward on the message the window shows, and
            // mailto: links clicked in it.
            cx.subscribe(&window.1, |_, request: &mail_ui::ComposeRequest, cx| {
                let request = request.clone();
                cx.defer(move |cx| compose::respond(request, cx));
            })
            .detach();
            cx.global_mut::<AppState>().window = Some(window);
        }
        Err(error) => {
            tracing::error!(%error, "could not open the main window");
            cx.quit();
        }
    }
}

/// Shows the settings tab in the main window and brings the window to the front.
fn open_settings(cx: &mut App) {
    let Some((handle, view)) = cx.global::<AppState>().window.clone() else {
        return;
    };
    let opened = handle.update(cx, |_, window, cx| {
        window.activate_window();
        view.update(cx, |view, cx| view.open_settings(false, window, cx));
    });
    if let Err(error) = opened {
        tracing::debug!(%error, "no main window for the settings");
    }
}

/// Tells the window what changed.
pub(crate) fn apply_changes(changes: &DataChanges, cx: &mut App) {
    if let Some((handle, view)) = cx.global::<AppState>().window.clone() {
        let applied = handle.update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.apply_changes(changes, window, cx));
        });
        if let Err(error) = applied {
            tracing::debug!(%error, "no main window for changes");
        }
    }
}

/// The main window, if one is open.
pub(crate) fn main_window(cx: &App) -> Option<AnyWindowHandle> {
    cx.try_global::<AppState>()
        .and_then(|state| state.window.as_ref())
        .map(|(handle, _)| *handle)
        .filter(|handle| cx.windows().contains(handle))
}

/// Handles the result of opening the core.
fn core_opened(result: Result<CoreHandle, CoreError>, cx: &mut App) {
    match result {
        Ok(core) => core_ready(&core, cx),
        Err(CoreError::MissingKeyForExistingData(lost)) => {
            fail(
                "Your mail is encrypted, but its key is missing. Enter your recovery key \
                 to unlock it, or start fresh.",
                cx,
            );
            lost_key::show(*lost, cx);
        }
        Err(error @ CoreError::AlreadyRunning { .. }) => {
            let message = startup::describe_open_error(&error);
            tracing::error!("{message}");
            fail(message.clone(), cx);
            let Some(window) = main_window(cx) else {
                cx.quit();
                return;
            };
            let answer = window.update(cx, |_, window, cx| {
                window.prompt(
                    PromptLevel::Critical,
                    &format!("{PRODUCT_NAME} is already running"),
                    Some(&message),
                    &["Quit"],
                    cx,
                )
            });
            match answer {
                Ok(answer) => cx
                    .spawn(async move |cx| {
                        let _ = answer.await;
                        cx.update(|cx| cx.quit());
                    })
                    .detach(),
                Err(_) => cx.quit(),
            }
        }
        Err(error) => {
            let message = startup::describe_open_error(&error);
            tracing::error!("{message}");
            fail(message, cx);
        }
    }
}

/// Shows `message` in place of the mail.
fn fail(message: impl Into<gpui::SharedString>, cx: &mut App) {
    cx.global::<AppState>().source.fail(message);
    apply_changes(&DataChanges::everything(), cx);
}

/// The core is open: show the mail, follow its events, serve the local API.
pub(crate) fn core_ready(core: &CoreHandle, cx: &mut App) {
    tracing::info!(data_dir = %core.data_dir().display(), "mail data opened");
    ui::gpui_tokio::init_from_handle(cx, core.runtime());
    let (source, authorizer) = {
        let state = cx.global_mut::<AppState>();
        state.core = Some(core.clone());
        (state.source.clone(), state.authorizer.clone())
    };
    source.attach(core.clone());
    apply_changes(&DataChanges::everything(), cx);
    follow_events(core.events(), source, cx);

    let approver = approve::test_auto_approver().unwrap_or_else(|| approval::start(cx));
    let services = api::services(core, &authorizer, approver);
    let (stop, stopped) = oneshot::channel::<()>();
    let paths = Paths::current();
    let task = core.runtime().spawn(async move {
        let paths = match paths {
            Ok(paths) => paths,
            Err(error) => {
                tracing::error!(%error, "local API not started");
                return;
            }
        };
        match LocalApiServer::bind(&paths, services, startup::server_config()).await {
            Ok(server) => {
                server
                    .serve(async {
                        let _ = stopped.await;
                    })
                    .await;
            }
            Err(error) => tracing::error!(%error, "the local API could not start"),
        }
    });
    cx.global_mut::<AppState>().server = Some(RunningServer { stop, task });

    updates::follow_core_settings(core, cx);
    theme::follow_core_settings(core, cx);
    reading::follow_core_settings(core, cx);
    compose::register_restore(core, cx);
    agents::start(core, &authorizer, cx);
    let window = cx
        .global::<AppState>()
        .window
        .as_ref()
        .map(|(_, view)| view.clone());
    updates::arm_crash_guard(core, window.as_ref(), cx);
    legacy_local::notify(core, cx);
    new_mail::start(core, cx);
}

/// Feeds the core's event batches into the window, merging what queued up meanwhile.
fn follow_events(
    mut events: broadcast::Receiver<CoreEventBatch>,
    source: Arc<CoreMailSource>,
    cx: &mut App,
) {
    let lost = || {
        let mut batch = CoreEventBatch::default();
        batch.reload = true;
        batch
    };
    cx.spawn(async move |cx| {
        loop {
            let batch = match events.recv().await {
                Ok(batch) => batch,
                Err(broadcast::error::RecvError::Lagged(_)) => lost(),
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let mut changes = source.changes(&batch);
            loop {
                match events.try_recv() {
                    Ok(batch) => changes.merge(source.changes(&batch)),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {
                        changes.merge(source.changes(&lost()));
                    }
                    Err(_) => break,
                }
            }
            if !changes.is_empty() {
                cx.update(|cx| apply_changes(&changes, cx));
            }
        }
    })
    .detach();
}

/// Stops the server and shuts the core down, waiting at most [`SHUTDOWN_TIMEOUT`].
fn shutdown(cx: &mut App) {
    let Some(state) = cx.try_global::<AppState>() else {
        return;
    };
    // The layout is saved a moment after each change; don't lose the last one.
    if let Some((_, window)) = state.window.clone() {
        window.update(cx, |window, cx| window.save_layout(cx));
    }
    let Some(state) = cx.try_global::<AppState>() else {
        return;
    };
    let Some(core) = state.core.clone() else {
        return;
    };
    let server = cx.global_mut::<AppState>().server.take();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    // A thread of its own: the core's runtime stops at the end of `shutdown`, and
    // GPUI's thread must not run Tokio futures.
    let spawned = std::thread::Builder::new()
        .name("mazemail-shutdown".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "cannot shut down cleanly");
                    return;
                }
            };
            runtime.block_on(async {
                if let Some(server) = server {
                    let _ = server.stop.send(());
                    let _ = server.task.await;
                }
                if let Err(error) = core.shutdown().await {
                    tracing::error!(%error, "shutting down the core");
                }
            });
            let _ = done_tx.send(());
        });
    match spawned {
        Ok(_) => {
            if done_rx.recv_timeout(SHUTDOWN_TIMEOUT).is_err() {
                tracing::warn!("shutdown took too long; quitting anyway");
            }
        }
        Err(error) => tracing::error!(%error, "cannot shut down cleanly"),
    }
}

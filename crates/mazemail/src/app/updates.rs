//! The auto-updater and the crash guard in the app (PLAN §4.15).
//!
//! - [`start`] creates the [`AutoUpdater`] at startup, before the mail data is open,
//!   so even a version that can't open its data can still update itself. It runs on a
//!   small Tokio runtime of its own ([`UpdateRuntime`]), because the app's Tokio
//!   bridge switches to the core's runtime once the core is open. The installer swaps
//!   the bundle the app runs from; when Maze runs translocated or can't write to its
//!   folder, updates are only announced and the button offers the `.dmg` ("Move Maze
//!   to Applications to get automatic updates").
//! - [`follow_core_settings`] applies the core's settings reloads live.
//! - [`arm_crash_guard`] calls `CoreHandle::mark_healthy` once the window painted with
//!   the core open (or a minute after the core opened, for windows that don't paint),
//!   and offers "Roll back to <previous version>" when the crash guard
//!   fired and the installer kept a previous bundle. The rollback swaps the bundle
//!   back; restoring the pre-migration database backup needs a store API that doesn't
//!   exist yet, so the database stays as it is.
//! - The "Updated to x.y.z" toast after the first launch of a new version.
//!
//! Neither `--headless` nor the CLI create an updater.
//!
//! Debug builds read two variables for trying the updater against a local release
//! server (see `cargo xtask update-e2e`): `MAZEMAIL_UPDATE_BASE_URL` replaces
//! `https://releases.mazemail.dev`, and `MAZEMAIL_UPDATE_SKIP_CODESIGN=1` accepts
//! unsigned bundles.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use auto_update::install::{STALE_AFTER, cleanup_downloads, cleanup_staging, restart_path};
use auto_update::{
    BundleInstaller, InstallError, InstallRequest, Installer, RollbackOffer, Updater,
    UpdaterConfig, rollback_offer, take_update_notice,
};
use auto_update_ui::{
    AutoUpdater, AutoUpdaterOptions, MOVE_TO_APPLICATIONS, UpdateButton, UpdateSettings,
};
use gpui::{App, AppContext as _, Entity, Global, PromptLevel, SharedString};
use mail_ui::MailWindow;
use mazemail_core::CoreHandle;
use mazemail_types::PRODUCT_NAME;
use release_channel::{AppVersion, ReleaseChannel};
use settings::Paths;

/// The updater's Tokio runtime, a GPUI global. Stopped without waiting at quit.
struct UpdateRuntime(Option<tokio::runtime::Runtime>);

impl Global for UpdateRuntime {}

impl Drop for UpdateRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// Stands in for the bundle installer when Maze can't replace its bundle. Never
/// called: such updaters only announce updates.
#[derive(Debug)]
struct NoInstaller(PathBuf);

impl Installer for NoInstaller {
    fn install(&self, _: &InstallRequest) -> Result<(), InstallError> {
        Err(InstallError::NotInBundle(self.0.clone()))
    }
}

/// How updates get installed in this process.
struct InstallTarget {
    installer: Arc<dyn Installer>,
    /// Why Maze can't install in place, if it can't.
    manual_install: Option<SharedString>,
    /// What to start after an update (the bundle, or its binary on Linux).
    restart_path: Option<PathBuf>,
}

/// Debug builds only: the manifest host for trying the updater locally.
const BASE_URL_ENV: &str = "MAZEMAIL_UPDATE_BASE_URL";
/// Debug builds only: accept unsigned bundles.
const SKIP_CODESIGN_ENV: &str = "MAZEMAIL_UPDATE_SKIP_CODESIGN";

fn debug_env(name: &str) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let value = std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    tracing::warn!("{name}={value} (debug build)");
    Some(value)
}

fn install_target(data_dir: &Path) -> InstallTarget {
    match BundleInstaller::for_running_app(data_dir) {
        Ok(installer) => {
            let restart_path = Some(restart_path(installer.installed_bundle()));
            let installer = if debug_env(SKIP_CODESIGN_ENV).as_deref() == Some("1") {
                installer.with_codesign(auto_update::CodesignCheck::Skip)
            } else {
                installer
            };
            match installer.check_writable() {
                Ok(()) => {
                    if let Some(parent) = installer.installed_bundle().parent() {
                        cleanup_staging(parent, STALE_AFTER);
                    }
                    InstallTarget {
                        installer: Arc::new(installer),
                        manual_install: None,
                        restart_path,
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "updates can't be installed in place");
                    InstallTarget {
                        installer: Arc::new(installer),
                        manual_install: Some(MOVE_TO_APPLICATIONS.into()),
                        restart_path,
                    }
                }
            }
        }
        Err(error) => {
            tracing::debug!(%error, "updates can't be installed in place");
            let exe = std::env::current_exe().unwrap_or_default();
            InstallTarget {
                installer: Arc::new(NoInstaller(exe)),
                manual_install: Some(match error {
                    InstallError::Translocated(_) => MOVE_TO_APPLICATIONS.into(),
                    other => other.to_string().into(),
                }),
                restart_path: None,
            }
        }
    }
}

/// Creates the app's updater and starts its background checks (if `auto_update` is
/// on and this build updates at all). Call once the main window is open.
pub(crate) fn start(paths: &Paths, cx: &mut App) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("mazemail-update")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "the updater could not start");
            return;
        }
    };
    let handle = runtime.handle().clone();
    cx.set_global(UpdateRuntime(Some(runtime)));

    let build = ReleaseChannel::current();
    let loaded = settings::load_settings(&paths.settings_file());
    let settings = UpdateSettings::from_settings(&loaded.settings, build);
    let updates_dir = paths.updates_dir();
    cleanup_downloads(&updates_dir, None, STALE_AFTER);

    let target = install_target(paths.data_dir());
    let mut config = UpdaterConfig::for_current_build(paths.data_dir(), settings.channel);
    config.install_mode = settings.install_mode;
    if let Some(url) = debug_env(BASE_URL_ENV) {
        config.manifest_base_url = url;
    }
    let updater = {
        let _runtime = handle.enter();
        match Updater::new(config, target.installer) {
            Ok(updater) => Arc::new(updater),
            Err(error) => {
                tracing::error!(%error, "the updater could not start");
                return;
            }
        }
    };
    let disabled = updater.disabled_reason();
    match &disabled {
        Some(reason) => tracing::info!(%reason, "updates are off"),
        None => tracing::info!(channel = settings.channel.name(), "updates are on"),
    }
    let options = AutoUpdaterOptions {
        manual_install: target.manual_install,
        restart_path: target.restart_path,
        ..AutoUpdaterOptions::default()
    };
    // Agent sessions (M6) register a restart guard here, so "Restart to update" asks
    // before ending them.
    let updater = cx.new(|cx| AutoUpdater::new(updater, handle, settings, options, cx));
    AutoUpdater::set_global(updater, cx);
    if let Some((_, window)) = cx.global::<super::AppState>().window.clone() {
        add_button(&window, cx);
    }

    // "Updated to x.y.z" once after an update (not for dev builds, which don't update).
    if build.receives_updates()
        && let Some(notice) = take_update_notice(&updates_dir, &AppVersion::current().semver)
    {
        tracing::info!(version = %notice.version, previous = %notice.previous, "updated");
        cx.defer(move |cx| auto_update_ui::show_update_notice(&notice, cx));
    }
}

/// Puts the updater's button into a main window's title bar.
pub(crate) fn add_button(window: &Entity<MailWindow>, cx: &mut App) {
    let Some(updater) = AutoUpdater::global(cx) else {
        return;
    };
    let button = cx.new(|cx| UpdateButton::new(updater, cx));
    window.update(cx, |window, cx| {
        window.add_title_bar_item(button.into(), cx)
    });
}

/// Applies the core's settings reloads (`auto_update`, `update_channel`,
/// `auto_update_install`) to the updater.
pub(crate) fn follow_core_settings(core: &CoreHandle, cx: &mut App) {
    if let Some(updater) = AutoUpdater::global(cx) {
        let settings = core.settings_watch();
        updater.update(cx, |updater, cx| {
            updater.follow_settings(settings, ReleaseChannel::current(), cx);
        });
    }
}

/// Marks this launch healthy once `window` painted with the core open, then offers a
/// rollback if the crash guard fired.
pub(crate) fn arm_crash_guard(
    core: &CoreHandle,
    window: Option<&Entity<MailWindow>>,
    cx: &mut App,
) {
    let offer = crash_rollback_offer(core);
    let mark = {
        let core = core.clone();
        move || {
            if let Err(error) = core.mark_healthy() {
                tracing::warn!(%error, "clearing the startup sentinel");
            }
        }
    };
    // A window that never paints (hidden at login, a locked screen) isn't a crash:
    // after a while without one, the launch counts as healthy too. The rollback offer
    // still waits for the window.
    let fallback = mark.clone();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(HEALTHY_AFTER).await;
        fallback();
    })
    .detach();
    let healthy = move |cx: &mut App| {
        mark();
        if let Some(offer) = offer {
            offer_rollback(offer, cx);
        }
    };
    match window {
        Some(window) => window.update(cx, |window, cx| window.after_next_paint(healthy, cx)),
        None => healthy(cx),
    }
}

/// A launch whose window didn't paint counts as healthy after this long with the core
/// open.
const HEALTHY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// A rollback to offer, if the crash guard fired and a previous bundle was kept.
fn crash_rollback_offer(core: &CoreHandle) -> Option<RollbackOffer> {
    if !core.should_offer_rollback() {
        return None;
    }
    let installer = match BundleInstaller::for_running_app(core.data_dir()) {
        Ok(installer) => installer,
        Err(error) => {
            tracing::warn!(%error, "the crash guard fired, but there is no bundle to roll back");
            return None;
        }
    };
    let offer = rollback_offer(
        true,
        installer.installed_bundle(),
        &core.data_dir().join("updates/previous"),
        &AppVersion::current().semver,
    );
    if offer.is_none() {
        tracing::warn!("the crash guard fired, but no previous version was kept");
    }
    offer
}

/// "Maze didn't start properly the last times. Roll back to Maze x.y.z?"
fn offer_rollback(offer: RollbackOffer, cx: &mut App) {
    let Some(window) = super::main_window(cx) else {
        return;
    };
    let label = offer.label();
    let detail = format!(
        "{PRODUCT_NAME} {} didn't start properly the last times it was opened. Your mail \
         stays as it is; {PRODUCT_NAME} restarts with the previous version.",
        AppVersion::current().semver
    );
    let answer = window.update(cx, |_, window, cx| {
        window.prompt(
            PromptLevel::Warning,
            &format!("{label}?"),
            Some(&detail),
            &[label.as_str(), "Keep This Version"],
            cx,
        )
    });
    let Ok(answer) = answer else {
        return;
    };
    cx.spawn(async move |cx| {
        if answer.await != Ok(0) {
            return;
        }
        let previous = offer.previous_bundle.clone();
        let installed = offer.installed_bundle.clone();
        let restored = cx
            .background_spawn(async move {
                auto_update::rollback::restore_previous_bundle(&previous, &installed)
            })
            .await;
        cx.update(|cx| match restored {
            Ok(()) => {
                cx.set_restart_path(restart_path(&offer.installed_bundle));
                cx.restart();
            }
            Err(error) => {
                tracing::error!(%error, "rolling back failed");
                if let Some(window) = super::main_window(cx) {
                    let message = format!("The rollback failed: {error}");
                    let _ = window.update(cx, |_, window, cx| {
                        drop(window.prompt(PromptLevel::Critical, &message, None, &["OK"], cx));
                    });
                }
            }
        });
    })
    .detach();
}

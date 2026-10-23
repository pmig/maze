//! [`AutoUpdater`], the GPUI entity around [`auto_update::Updater`].

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use auto_update::{
    CheckOutcome, DEFAULT_POLL_INTERVAL, InstallMode, UpdateError, UpdateStatus, Updater,
};
use gpui::{
    App, AppContext as _, Context, Entity, Global, PromptLevel, SharedString, Task, Window,
};
use gpui_kit::component::{Sizable as _, button::Button, notification::Notification};
use mazemail_types::PRODUCT_NAME;
use release_channel::ReleaseChannel;
use semver::Version;
use settings::LoadedSettings;
use tokio::sync::watch;
use ui::gpui_tokio::{JoinError, spawn_on};

use crate::settings::UpdateSettings;
use crate::state::{StateContext, TitleBarState, title_bar_state};
use crate::{MOVE_TO_APPLICATIONS, notify};

/// Decides whether a restart needs asking first: returns the question ("An agent
/// session is running. Restart anyway?") or `None` to restart right away.
pub type RestartGuard = Rc<dyn Fn(&App) -> Option<SharedString>>;

/// How the [`AutoUpdater`] runs, besides the settings.
#[derive(Debug, Clone)]
pub struct AutoUpdaterOptions {
    /// Set when Maze can't replace its own bundle (it runs translocated, or the app
    /// folder isn't writable): the reason. Updates are then only announced, and the
    /// button opens the `.dmg` (PLAN §4.15).
    pub manual_install: Option<SharedString>,
    /// What "Restart to update" starts: the installed bundle on macOS, its binary on
    /// Linux (`auto_update::install::restart_path`). `None`: the running app.
    pub restart_path: Option<PathBuf>,
    /// Time between background checks.
    pub poll_interval: Duration,
}

impl Default for AutoUpdaterOptions {
    fn default() -> Self {
        Self {
            manual_install: None,
            restart_path: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

/// The app's updater: follows the [`Updater`]'s status for the title bar, polls
/// while `auto_update` is on, applies settings changes live, and restarts into the new
/// version. The Tokio work runs on `runtime`, which must outlive the entity.
pub struct AutoUpdater {
    updater: Arc<Updater>,
    runtime: tokio::runtime::Handle,
    status: UpdateStatus,
    settings: UpdateSettings,
    options: AutoUpdaterOptions,
    manual_check: bool,
    restart_guard: Option<RestartGuard>,
    announced_manual: Option<Version>,
    poll: Option<Task<Result<(), JoinError>>>,
    work: Option<Task<()>>,
    _status: Task<()>,
    settings_task: Option<Task<()>>,
}

impl std::fmt::Debug for AutoUpdater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoUpdater")
            .field("status", &self.status)
            .field("settings", &self.settings)
            .field("polling", &self.poll.is_some())
            .finish_non_exhaustive()
    }
}

struct GlobalAutoUpdater(Entity<AutoUpdater>);

impl Global for GlobalAutoUpdater {}

impl AutoUpdater {
    /// Wraps `updater`, starts following its status and applies `settings` (which
    /// starts the background checks if they're on and updates are possible).
    pub fn new(
        updater: Arc<Updater>,
        runtime: tokio::runtime::Handle,
        settings: UpdateSettings,
        options: AutoUpdaterOptions,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let mut receiver = updater.subscribe();
        let status = receiver.borrow_and_update().clone();
        // `watch` needs no Tokio runtime on the receiving side.
        let status_task = cx.spawn(async move |this, cx| {
            while receiver.changed().await.is_ok() {
                let status = receiver.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| this.set_status(status, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut this = Self {
            updater,
            runtime,
            status,
            settings,
            options,
            manual_check: false,
            restart_guard: None,
            announced_manual: None,
            poll: None,
            work: None,
            _status: status_task,
            settings_task: None,
        };
        this.apply_settings(settings, cx);
        this
    }

    /// Makes `updater` the app's updater (for "Maze: Check for Updates").
    pub fn set_global(updater: Entity<Self>, cx: &mut App) {
        cx.set_global(GlobalAutoUpdater(updater));
    }

    /// The app's updater, if one was set.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalAutoUpdater>()
            .map(|global| global.0.clone())
    }

    /// The updater's last status.
    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    /// The settings in effect.
    pub fn settings(&self) -> UpdateSettings {
        self.settings
    }

    /// Why updates are disabled for this build, if they are.
    pub fn disabled_reason(&self) -> Option<String> {
        self.updater.disabled_reason()
    }

    /// Why Maze can't install updates in place, if it can't.
    pub fn manual_install(&self) -> Option<&SharedString> {
        self.options.manual_install.as_ref()
    }

    /// Whether background checks are running.
    pub fn is_polling(&self) -> bool {
        self.poll.is_some()
    }

    /// The install mode in effect: the setting, or `notify_only` if Maze can't install
    /// in place.
    pub fn effective_install_mode(&self) -> InstallMode {
        if self.options.manual_install.is_some() {
            InstallMode::NotifyOnly
        } else {
            self.settings.install_mode
        }
    }

    /// What the title bar shows.
    pub fn title_bar_state(&self) -> TitleBarState {
        title_bar_state(
            &self.status,
            StateContext {
                manual_check: self.manual_check,
                manual_install: self.options.manual_install.is_some(),
            },
        )
    }

    /// Asks `guard` before every restart (agent sessions would end, PLAN §4.15).
    pub fn set_restart_guard(&mut self, guard: RestartGuard) {
        self.restart_guard = Some(guard);
    }

    /// Applies new settings: the channel and install mode take effect at the next
    /// check; turning `auto_update` on or off starts or stops the background checks,
    /// and a changed channel or mode checks again right away.
    pub fn apply_settings(&mut self, settings: UpdateSettings, cx: &mut Context<'_, Self>) {
        let changed = settings != self.settings;
        self.settings = settings;
        self.updater
            .set_options(settings.channel, self.effective_install_mode());
        let should_poll = settings.auto_check && self.updater.disabled_reason().is_none();
        if !should_poll {
            if self.poll.take().is_some() {
                tracing::info!("background update checks are off");
            }
        } else if self.poll.is_none() || changed {
            self.start_polling(cx);
        }
        cx.notify();
    }

    /// Applies every settings reload from `settings` (the core's settings watcher),
    /// starting with its current value.
    pub fn follow_settings(
        &mut self,
        mut settings: watch::Receiver<Arc<LoadedSettings>>,
        build: ReleaseChannel,
        cx: &mut Context<'_, Self>,
    ) {
        self.settings_task = Some(cx.spawn(async move |this, cx| {
            loop {
                let resolved = {
                    let loaded = settings.borrow_and_update();
                    UpdateSettings::from_settings(&loaded.settings, build)
                };
                if this
                    .update(cx, |this, cx| this.apply_settings(resolved, cx))
                    .is_err()
                {
                    break;
                }
                if settings.changed().await.is_err() {
                    break;
                }
            }
        }));
    }

    fn start_polling(&mut self, cx: &mut Context<'_, Self>) {
        let updater = self.updater.clone();
        let interval = self.options.poll_interval;
        tracing::info!(
            channel = self.settings.channel.name(),
            interval_secs = interval.as_secs(),
            "checking for updates in the background"
        );
        self.poll = Some(spawn_on(&self.runtime, cx, async move {
            updater.poll(interval).await;
        }));
    }

    /// Checks now, because the user asked. Disabled builds return the reason as
    /// [`UpdateError::Disabled`] without checking.
    pub fn check(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> Task<Result<CheckOutcome, Arc<UpdateError>>> {
        if let Some(reason) = self.disabled_reason() {
            return Task::ready(Err(Arc::new(UpdateError::Disabled(reason))));
        }
        self.manual_check = true;
        cx.notify();
        let updater = self.updater.clone();
        let task = spawn_on(&self.runtime, cx, async move {
            updater.check_and_download().await
        });
        cx.background_spawn(async move {
            task.await.unwrap_or_else(|err| {
                Err(Arc::new(UpdateError::Protocol(format!(
                    "the update check stopped: {err}"
                ))))
            })
        })
    }

    /// The "Update to x.y.z" button: installs the offered version, or opens its `.dmg`
    /// if Maze can't install in place.
    pub fn install_available(&mut self, cx: &mut Context<'_, Self>) {
        if self.options.manual_install.is_some() {
            self.open_dmg(cx);
            return;
        }
        let updater = self.updater.clone();
        let task = spawn_on(&self.runtime, cx, async move {
            updater.check_and_install().await
        });
        self.work = Some(cx.background_spawn(async move {
            match task.await {
                Ok(Ok(outcome)) => tracing::info!(?outcome, "update installed"),
                Ok(Err(err)) => tracing::warn!(%err, "installing the update failed"),
                Err(err) => tracing::warn!(%err, "the install task stopped"),
            }
        }));
    }

    /// Opens the `.dmg` of the offered version (the download of the browser), or its
    /// release notes if there is no `.dmg`.
    pub fn open_dmg(&self, cx: &mut App) {
        let Some(manifest) = self.updater.offered_manifest() else {
            return;
        };
        let url = manifest.dmg_url().unwrap_or(manifest.notes_url);
        cx.open_url(url.as_str());
    }

    /// Clears an error or an "Update to" notice.
    pub fn dismiss(&mut self, cx: &mut Context<'_, Self>) {
        self.manual_check = false;
        self.updater.dismiss();
        cx.notify();
    }

    /// "Restart to update": asks the restart guard first, then restarts into the
    /// installed bundle.
    pub fn restart(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let path = self.options.restart_path.clone();
        let question = self.restart_guard.as_ref().and_then(|guard| guard(cx));
        let Some(question) = question else {
            restart_app(path, cx);
            return;
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &question,
            None,
            &["Restart", "Cancel"],
            cx,
        );
        cx.spawn(async move |_, cx| {
            if answer.await == Ok(0) {
                cx.update(|cx| restart_app(path, cx));
            }
        })
        .detach();
    }

    fn set_status(&mut self, status: UpdateStatus, cx: &mut Context<'_, Self>) {
        // A check the user started ends with its result; later background checks are
        // background checks again.
        if !matches!(
            status,
            UpdateStatus::Checking | UpdateStatus::Errored { .. }
        ) {
            self.manual_check = false;
        }
        if let UpdateStatus::Available { version } = &status
            && self.options.manual_install.is_some()
            && self.announced_manual.as_ref() != Some(version)
        {
            self.announced_manual = Some(version.clone());
            self.announce_manual_update(version, cx);
        }
        self.status = status;
        cx.notify();
    }

    /// "Maze x.y.z is available. Move Maze to Applications…", once per version.
    fn announce_manual_update(&self, version: &Version, cx: &mut Context<'_, Self>) {
        let this = cx.weak_entity();
        let note = Notification::info(format!(
            "{PRODUCT_NAME} {version} is available. {MOVE_TO_APPLICATIONS}"
        ))
        .title(PRODUCT_NAME)
        .action(move |_, _, _| {
            let this = this.clone();
            Button::new("download-update")
                .small()
                .label("Download")
                .on_click(move |_, _, cx| {
                    this.update(cx, |this, cx| this.open_dmg(cx)).ok();
                })
        });
        cx.defer(move |cx| notify(cx, note));
    }
}

fn restart_app(path: Option<PathBuf>, cx: &mut App) {
    tracing::info!("restarting to update");
    if let Some(path) = path {
        cx.set_restart_path(path);
    }
    cx.restart();
}

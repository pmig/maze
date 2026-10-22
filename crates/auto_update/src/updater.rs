//! The update state machine (adapted from Zed's `auto_update` crate,
//! `crates/auto_update/src/auto_update.rs`, GPL-3.0-or-later, without GPUI).
//!
//! `Idle → Checking → Downloading { version, progress } → Installing { version } →
//! Updated { version }`, plus `Errored`, and `Available` for `notify_only` mode. The
//! status is published on a `tokio::sync::watch` channel for the UI.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::VerifyingKey;
use release_channel::{AppVersion, ReleaseChannel};
use semver::Version;
use tokio::sync::watch;

use crate::download::download_artifact;
use crate::install::{InstallError, InstallRequest, Installer};
use crate::manifest::{
    DEFAULT_MANIFEST_BASE_URL, Manifest, ManifestError, Platform, UpdateDecision, check_manifest,
    manifest_url, verify_manifest,
};
use crate::{embedded_public_keys, update_explanation};

/// Manifests larger than this are rejected without parsing.
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// How often to check by default (PLAN §4.15: every hour).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// The status of the updater.
#[derive(Debug, Clone)]
pub enum UpdateStatus {
    /// Nothing happening.
    Idle,
    /// Fetching and verifying the manifest.
    Checking,
    /// A newer version was found and is not downloaded (`notify_only` mode).
    Available {
        /// The offered version.
        version: Version,
    },
    /// Downloading the artifact.
    Downloading {
        /// The version being downloaded.
        version: Version,
        /// Progress in `0.0..=1.0`, `None` before the first bytes arrive.
        progress: Option<f32>,
    },
    /// Installing; never interrupted.
    Installing {
        /// The version being installed.
        version: Version,
    },
    /// Installed; restart to run it.
    Updated {
        /// The installed version.
        version: Version,
    },
    /// The last attempt failed.
    Errored {
        /// What went wrong.
        error: Arc<UpdateError>,
    },
}

impl PartialEq for UpdateStatus {
    fn eq(&self, other: &Self) -> bool {
        use UpdateStatus::*;
        match (self, other) {
            (Idle, Idle) | (Checking, Checking) => true,
            (Available { version: a }, Available { version: b })
            | (Installing { version: a }, Installing { version: b })
            | (Updated { version: a }, Updated { version: b }) => a == b,
            (
                Downloading {
                    version: a,
                    progress: pa,
                },
                Downloading {
                    version: b,
                    progress: pb,
                },
            ) => a == b && pa == pb,
            (Errored { error: a }, Errored { error: b }) => a.to_string() == b.to_string(),
            _ => false,
        }
    }
}

impl UpdateStatus {
    /// Whether an update is installed and waits for a restart.
    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }

    /// Whether a check, download or install is running.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking | Self::Downloading { .. } | Self::Installing { .. }
        )
    }
}

/// Why an update failed.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// Updates are disabled for this build.
    #[error("{0}")]
    Disabled(String),
    /// A network error.
    #[error("network error: {0}")]
    Http(String),
    /// The server answered with an error status.
    #[error("{url} returned HTTP {status}")]
    HttpStatus {
        /// The requested URL.
        url: String,
        /// The status code.
        status: u16,
    },
    /// The server's answer didn't make sense.
    #[error("{0}")]
    Protocol(String),
    /// The manifest was rejected.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// The database is too old for the offered release.
    #[error(
        "this update needs database schema {required} or newer (installed: {installed}); \
         install an intermediate release first"
    )]
    SchemaTooOld {
        /// `min_schema_from` of the release.
        required: u32,
        /// The installed schema.
        installed: u32,
    },
    /// The artifact has the wrong size.
    #[error("the download has {actual} bytes, expected {expected}")]
    SizeMismatch {
        /// Size from the manifest.
        expected: u64,
        /// Size received.
        actual: u64,
    },
    /// The artifact has the wrong SHA-256.
    #[error("the download's SHA-256 is {actual}, expected {expected}")]
    HashMismatch {
        /// Hash from the manifest.
        expected: String,
        /// Hash of the download.
        actual: String,
    },
    /// A file system error.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// Installing failed.
    #[error(transparent)]
    Install(#[from] InstallError),
}

impl UpdateError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    pub(crate) fn http(err: reqwest::Error) -> Self {
        // Without the URL's query, which we never set anyway.
        Self::Http(err.without_url().to_string())
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// No newer version (or none for this platform yet).
    UpToDate,
    /// A newer version exists; not downloaded because of [`InstallMode::NotifyOnly`].
    Available(Version),
    /// A newer version was downloaded and installed; restart to use it.
    Installed(Version),
    /// Another check is running.
    Busy,
}

/// What to do with a newer version.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InstallMode {
    /// Download and install in the background (`"auto_update_install": "background"`).
    #[default]
    Background,
    /// Only report it (`"notify_only"`).
    NotifyOnly,
}

/// Configuration of an [`Updater`].
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// The running version.
    pub current_version: Version,
    /// The channel of the running build. Dev builds never update.
    pub build_channel: ReleaseChannel,
    /// The channel to follow (`update_channel` setting).
    pub channel: ReleaseChannel,
    /// What to do with a newer version.
    pub install_mode: InstallMode,
    /// Manifest host, `https://releases.mazemail.dev` unless testing.
    pub manifest_base_url: String,
    /// The platform to fetch manifests for.
    pub platform: Platform,
    /// Keys trusted to sign manifests.
    pub public_keys: Vec<VerifyingKey>,
    /// `<data_dir>/updates`; downloads go to `<updates_dir>/<version>/`.
    pub updates_dir: PathBuf,
    /// The installed database schema, checked against `min_schema_from`. `None`:
    /// unknown (no database yet), no check.
    pub schema_version: Option<u32>,
    /// Set: updates are disabled with this explanation (from
    /// `MAZEMAIL_UPDATE_EXPLANATION` by default).
    pub disabled_reason: Option<String>,
}

impl UpdaterConfig {
    /// The configuration of the running build: its version and channel, the embedded
    /// keys, the default manifest host and this platform.
    pub fn for_current_build(data_dir: &Path, channel: ReleaseChannel) -> Self {
        let app = AppVersion::current();
        Self {
            current_version: app.semver.clone(),
            build_channel: app.channel,
            channel,
            install_mode: InstallMode::default(),
            manifest_base_url: DEFAULT_MANIFEST_BASE_URL.to_owned(),
            platform: Platform::current(),
            public_keys: embedded_public_keys(),
            updates_dir: data_dir.join("updates"),
            schema_version: None,
            disabled_reason: update_explanation().map(str::to_owned),
        }
    }

    /// Why updates can't run, if they can't.
    fn disabled(&self) -> Option<String> {
        if let Some(reason) = &self.disabled_reason {
            return Some(reason.clone());
        }
        if !self.build_channel.receives_updates() {
            return Some(format!(
                "{} builds don't update themselves",
                self.build_channel.app_name()
            ));
        }
        if !self.channel.receives_updates() {
            return Some("there are no updates for the dev channel".to_owned());
        }
        if self.public_keys.is_empty() {
            return Some("this build has no update signing keys".to_owned());
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct Options {
    channel: ReleaseChannel,
    install_mode: InstallMode,
}

/// Checks for, downloads and installs updates. Needs a Tokio runtime.
#[derive(Debug)]
pub struct Updater {
    config: UpdaterConfig,
    options: Mutex<Options>,
    client: reqwest::Client,
    installer: Arc<dyn Installer>,
    status: watch::Sender<UpdateStatus>,
    run_lock: tokio::sync::Mutex<()>,
}

impl Updater {
    /// Creates an updater. The HTTP client sends `User-Agent: mazemail/<version>` and
    /// nothing else identifying.
    pub fn new(config: UpdaterConfig, installer: Arc<dyn Installer>) -> Result<Self, UpdateError> {
        let https_only = config.manifest_base_url.starts_with("https://");
        let client = reqwest::Client::builder()
            .user_agent(format!("mazemail/{}", config.current_version))
            .referer(false)
            .https_only(https_only)
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(UpdateError::http)?;
        Ok(Self {
            options: Mutex::new(Options {
                channel: config.channel,
                install_mode: config.install_mode,
            }),
            config,
            client,
            installer,
            status: watch::Sender::new(UpdateStatus::Idle),
            run_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The current status.
    pub fn status(&self) -> UpdateStatus {
        self.status.borrow().clone()
    }

    /// Status updates, for the UI. Works without a Tokio runtime on the receiving side.
    pub fn subscribe(&self) -> watch::Receiver<UpdateStatus> {
        self.status.subscribe()
    }

    /// Why updates are disabled, if they are.
    pub fn disabled_reason(&self) -> Option<String> {
        self.config.disabled()
    }

    /// Changes the channel and install mode (settings changed). Takes effect at the next
    /// check.
    pub fn set_options(&self, channel: ReleaseChannel, install_mode: InstallMode) {
        if let Ok(mut options) = self.options.lock() {
            *options = Options {
                channel,
                install_mode,
            };
        }
    }

    /// Clears an error or an `Available` notice (the user dismissed it).
    pub fn dismiss(&self) {
        self.status.send_if_modified(|status| {
            if matches!(
                status,
                UpdateStatus::Errored { .. } | UpdateStatus::Available { .. }
            ) {
                *status = UpdateStatus::Idle;
                true
            } else {
                false
            }
        });
    }

    /// Checks hourly (or at `interval`) for as long as the future is polled.
    /// Errors are reported through the status.
    pub async fn poll(&self, interval: Duration) {
        loop {
            if let Err(err) = self.check_and_download().await {
                tracing::warn!(%err, "update check failed");
            }
            tokio::time::sleep(interval).await;
        }
    }

    /// Checks for an update using the configured install mode.
    pub async fn check_and_download(&self) -> Result<CheckOutcome, Arc<UpdateError>> {
        let mode = self.options().install_mode;
        self.check_with(mode).await
    }

    /// Checks for an update and installs it regardless of the install mode (the user
    /// clicked "Update" on an `Available` notice).
    pub async fn check_and_install(&self) -> Result<CheckOutcome, Arc<UpdateError>> {
        self.check_with(InstallMode::Background).await
    }

    fn options(&self) -> Options {
        self.options.lock().map_or(
            Options {
                channel: self.config.channel,
                install_mode: self.config.install_mode,
            },
            |options| *options,
        )
    }

    async fn check_with(&self, mode: InstallMode) -> Result<CheckOutcome, Arc<UpdateError>> {
        if let Some(reason) = self.config.disabled() {
            return Err(Arc::new(UpdateError::Disabled(reason)));
        }
        let Ok(_running) = self.run_lock.try_lock() else {
            return Ok(CheckOutcome::Busy);
        };
        let previous = self.status();
        if matches!(previous, UpdateStatus::Installing { .. }) {
            return Ok(CheckOutcome::Busy);
        }

        // If the future is dropped (e.g. on sleep), the network phases go back to the
        // previous status; an install that started finishes on its own thread.
        let mut guard = PhaseGuard {
            status: &self.status,
            restore: Some(match &previous {
                UpdateStatus::Updated { .. } | UpdateStatus::Available { .. } => previous.clone(),
                _ => UpdateStatus::Idle,
            }),
        };
        self.status.send_replace(UpdateStatus::Checking);

        let result = self.run(mode, &previous, &mut guard).await;
        match result {
            Ok(outcome) => Ok(outcome),
            Err(err) => {
                let err = Arc::new(err);
                guard.restore = None;
                self.status
                    .send_replace(UpdateStatus::Errored { error: err.clone() });
                Err(err)
            }
        }
    }

    async fn run(
        &self,
        mode: InstallMode,
        previous: &UpdateStatus,
        guard: &mut PhaseGuard<'_>,
    ) -> Result<CheckOutcome, UpdateError> {
        let options = self.options();
        let Some(manifest) = self.fetch_manifest(options.channel).await? else {
            guard.finish();
            return Ok(CheckOutcome::UpToDate);
        };

        // After an install, compare against the installed (not the running) version,
        // so the same update isn't downloaded twice.
        let installed = match previous {
            UpdateStatus::Updated { version } => version,
            _ => &self.config.current_version,
        };
        let manifest =
            match check_manifest(manifest, options.channel, &self.config.platform, installed) {
                Ok(UpdateDecision::Newer(manifest)) => *manifest,
                Ok(UpdateDecision::UpToDate) => {
                    guard.finish();
                    return Ok(CheckOutcome::UpToDate);
                }
                // Switching from preview to an older stable is not an error worth showing.
                Err(ManifestError::Downgrade { current, offered }) => {
                    tracing::info!(%current, %offered, "not downgrading");
                    guard.finish();
                    return Ok(CheckOutcome::UpToDate);
                }
                Err(err) => return Err(err.into()),
            };

        if let Some(installed) = self.config.schema_version
            && installed < manifest.min_schema_from
        {
            return Err(UpdateError::SchemaTooOld {
                required: manifest.min_schema_from,
                installed,
            });
        }

        let version = manifest.version.clone();
        if mode == InstallMode::NotifyOnly {
            guard.restore = None;
            self.status.send_replace(UpdateStatus::Available {
                version: version.clone(),
            });
            return Ok(CheckOutcome::Available(version));
        }

        self.status.send_replace(UpdateStatus::Downloading {
            version: version.clone(),
            progress: None,
        });
        let artifact = self.download(&manifest).await?;

        // Installing: from here on the work runs on a blocking thread that also
        // publishes the final status, so dropping this future can't interrupt it.
        guard.restore = None;
        self.status.send_replace(UpdateStatus::Installing {
            version: version.clone(),
        });
        let installer = self.installer.clone();
        let status = self.status.clone();
        let download_dir = self.config.updates_dir.join(version.to_string());
        let request = InstallRequest {
            artifact,
            version: version.clone(),
        };
        let task = tokio::task::spawn_blocking(move || {
            let result = installer.install(&request);
            if result.is_ok() {
                if let Err(err) = std::fs::remove_dir_all(&download_dir) {
                    tracing::debug!(%err, "cannot remove the downloaded update");
                }
                status.send_replace(UpdateStatus::Updated {
                    version: request.version.clone(),
                });
            }
            result
        });
        match task.await {
            Ok(Ok(())) => Ok(CheckOutcome::Installed(version)),
            Ok(Err(err)) => Err(err.into()),
            Err(err) => Err(UpdateError::Protocol(format!("install task failed: {err}"))),
        }
    }

    /// Fetches the signed manifest. `None` if there is none for this platform yet.
    async fn fetch_manifest(
        &self,
        channel: ReleaseChannel,
    ) -> Result<Option<Manifest>, UpdateError> {
        let url = manifest_url(
            &self.config.manifest_base_url,
            channel,
            &self.config.platform,
        );
        tracing::debug!(%url, "checking for updates");
        let mut response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(UpdateError::http)?;
        match response.status() {
            reqwest::StatusCode::OK => {}
            reqwest::StatusCode::NOT_FOUND => {
                tracing::info!(%url, "no update manifest for this platform");
                return Ok(None);
            }
            status => {
                return Err(UpdateError::HttpStatus {
                    url,
                    status: status.as_u16(),
                });
            }
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(UpdateError::http)? {
            if body.len() + chunk.len() > MAX_MANIFEST_BYTES {
                return Err(UpdateError::Protocol(
                    "the update manifest is too large".to_owned(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(verify_manifest(&body, &self.config.public_keys)?))
    }

    async fn download(&self, manifest: &Manifest) -> Result<PathBuf, UpdateError> {
        let dir = self.config.updates_dir.join(manifest.version.to_string());
        let version = manifest.version.clone();
        let status = &self.status;
        download_artifact(&self.client, manifest, &dir, |fraction| {
            status.send_replace(UpdateStatus::Downloading {
                version: version.clone(),
                progress: Some(fraction),
            });
        })
        .await
    }
}

/// Restores the status if the check is dropped during a network phase.
struct PhaseGuard<'a> {
    status: &'a watch::Sender<UpdateStatus>,
    restore: Option<UpdateStatus>,
}

impl PhaseGuard<'_> {
    /// The check ended without a new status of its own: restore the previous one.
    fn finish(&mut self) {
        if let Some(status) = self.restore.take() {
            self.status.send_replace(status);
        }
    }
}

impl Drop for PhaseGuard<'_> {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            self.status.send_if_modified(|status| {
                if matches!(
                    status,
                    UpdateStatus::Checking | UpdateStatus::Downloading { .. }
                ) {
                    *status = restore;
                    true
                } else {
                    false
                }
            });
        }
    }
}

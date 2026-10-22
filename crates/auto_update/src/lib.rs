//! Maze auto-update core, without GPUI (PLAN §4.15).
//!
//! - [`manifest`]: the signed release manifest, its canonical JSON, Ed25519
//!   verification against the embedded keys (two, for rotation) and the version
//!   policy (channel, platform, no downgrades, semver prerelease ordering).
//! - [`Updater`]: the state machine `Idle → Checking → Downloading → Installing →
//!   Updated` (+ `Errored`), driven by [`Updater::check_and_download`], published on a
//!   `tokio::sync::watch` channel for the GPUI layer (`auto_update_ui`).
//! - Downloads are resumable (HTTP `Range`) into `<data_dir>/updates/<version>/` and
//!   checked against the manifest's size and SHA-256. The only request header that
//!   identifies anything is `User-Agent: mazemail/<version>`.
//! - [`install`]: extract next to the app, check the code signature (macOS), swap the
//!   bundle atomically, keep the previous bundle for rollback.
//!
//! Builds made with `MAZEMAIL_UPDATE_EXPLANATION` set (distribution packages) never
//! update and show that message instead ([`update_explanation`]).

pub mod install;
pub mod keys;
pub mod manifest;

mod download;
mod updater;

pub use download::sha256_file;
pub use install::{BundleInstaller, CodesignCheck, InstallError, InstallRequest, Installer};
pub use keys::{KeyError, embedded_public_keys};
pub use manifest::{Manifest, ManifestError, Platform, UpdateDecision};
pub use updater::{
    CheckOutcome, DEFAULT_POLL_INTERVAL, InstallMode, UpdateError, UpdateStatus, Updater,
    UpdaterConfig,
};

/// Why this build doesn't update itself, if it was built with
/// `MAZEMAIL_UPDATE_EXPLANATION` (e.g. "managed by your package manager").
pub fn update_explanation() -> Option<&'static str> {
    option_env!("MAZEMAIL_UPDATE_EXPLANATION").filter(|reason| !reason.trim().is_empty())
}

//! The "Updated to x.y.z — release notes" toast after a restart (PLAN §4.15).
//!
//! Two small files in `<data_dir>/updates/`, neither with any mail data:
//!
//! - [`INSTALLED_FILE`] (`installed.json`): written by the [`Updater`](crate::Updater)
//!   after an install, with the version and the manifest's `notes_url`, because the
//!   manifest is gone after the restart;
//! - [`LAST_VERSION_FILE`] (`last-version`): the version that ran last. A launch of a
//!   newer version than that is a launch after an update.

use std::fs;
use std::io;
use std::path::Path;

use semver::Version;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::manifest::compare_versions;

/// What the updater installed, for the toast after the restart.
pub const INSTALLED_FILE: &str = "installed.json";

/// The version that ran last.
pub const LAST_VERSION_FILE: &str = "last-version";

/// An installed update (the content of [`INSTALLED_FILE`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledUpdate {
    /// The installed version.
    pub version: Version,
    /// Its release notes.
    pub notes_url: Option<Url>,
}

/// Remembers an installed update. Replaces the file atomically.
pub fn record_installed(updates_dir: &Path, update: &InstalledUpdate) -> io::Result<()> {
    fs::create_dir_all(updates_dir)?;
    let bytes = serde_json::to_vec_pretty(update).map_err(io::Error::other)?;
    write_atomic(&updates_dir.join(INSTALLED_FILE), &bytes)
}

/// "Updated to x.y.z", shown once after the first launch of a new version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateNotice {
    /// The version now running.
    pub version: Version,
    /// The version that ran before.
    pub previous: Version,
    /// The release notes, if the updater recorded them.
    pub notes_url: Option<Url>,
}

/// Call once per GUI launch: returns a notice if `current` is newer than the version
/// that ran last, and records `current` as the last version. A first launch (no
/// record yet) and a downgrade (rollback) return `None`.
pub fn take_update_notice(updates_dir: &Path, current: &Version) -> Option<UpdateNotice> {
    let last_path = updates_dir.join(LAST_VERSION_FILE);
    let last = fs::read_to_string(&last_path)
        .ok()
        .and_then(|text| Version::parse(text.trim()).ok());
    if last.as_ref() != Some(current) {
        let written = fs::create_dir_all(updates_dir)
            .and_then(|()| write_atomic(&last_path, current.to_string().as_bytes()));
        if let Err(err) = written {
            tracing::warn!(%err, "cannot record the running version");
        }
    }

    let installed_path = updates_dir.join(INSTALLED_FILE);
    let installed = fs::read(&installed_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<InstalledUpdate>(&bytes).ok());
    // Consumed once the recorded version (or a newer one) runs.
    if installed
        .as_ref()
        .is_some_and(|update| compare_versions(&update.version, current).is_le())
        && let Err(err) = fs::remove_file(&installed_path)
    {
        tracing::debug!(%err, "cannot remove {INSTALLED_FILE}");
    }

    let previous = last?;
    if !compare_versions(current, &previous).is_gt() {
        return None;
    }
    let notes_url = installed
        .filter(|update| &update.version == current)
        .and_then(|update| update.notes_url);
    Some(UpdateNotice {
        version: current.clone(),
        previous,
        notes_url,
    })
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn notices_once_after_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path().join("updates");

        // First launch ever: nothing to announce.
        assert_eq!(take_update_notice(&updates, &v("0.1.0-alpha.1")), None);
        assert_eq!(take_update_notice(&updates, &v("0.1.0-alpha.1")), None);

        // The updater installed alpha.2 and recorded its notes.
        let notes =
            Url::parse("https://github.com/pmig/maze/releases/tag/0.1.0-alpha.2").unwrap();
        record_installed(
            &updates,
            &InstalledUpdate {
                version: v("0.1.0-alpha.2"),
                notes_url: Some(notes.clone()),
            },
        )
        .unwrap();
        // The old version keeps running until the restart: no notice yet, and the
        // record stays.
        assert_eq!(take_update_notice(&updates, &v("0.1.0-alpha.1")), None);
        assert!(updates.join(INSTALLED_FILE).exists());

        assert_eq!(
            take_update_notice(&updates, &v("0.1.0-alpha.2")),
            Some(UpdateNotice {
                version: v("0.1.0-alpha.2"),
                previous: v("0.1.0-alpha.1"),
                notes_url: Some(notes),
            })
        );
        assert!(!updates.join(INSTALLED_FILE).exists());
        // Only once.
        assert_eq!(take_update_notice(&updates, &v("0.1.0-alpha.2")), None);
    }

    #[test]
    fn notices_without_notes_and_ignores_rollbacks() {
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path();
        take_update_notice(updates, &v("1.0.0"));
        // Updated by hand (a new .dmg): no recorded notes.
        let notice = take_update_notice(updates, &v("1.1.0")).unwrap();
        assert_eq!(notice.notes_url, None);
        // Rolled back: no "Updated to" toast.
        assert_eq!(take_update_notice(updates, &v("1.0.0")), None);
        assert_eq!(
            fs::read_to_string(updates.join(LAST_VERSION_FILE)).unwrap(),
            "1.0.0"
        );
        // A garbled record counts as no record.
        fs::write(updates.join(LAST_VERSION_FILE), "garbage").unwrap();
        assert_eq!(take_update_notice(updates, &v("1.1.0")), None);
    }

    #[test]
    fn notes_of_another_version_are_not_shown() {
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path();
        take_update_notice(updates, &v("1.0.0"));
        record_installed(
            updates,
            &InstalledUpdate {
                version: v("1.2.0"),
                notes_url: Some(Url::parse("https://example.com/1.2.0").unwrap()),
            },
        )
        .unwrap();
        let notice = take_update_notice(updates, &v("1.1.0")).unwrap();
        assert_eq!(notice.notes_url, None);
        // 1.2.0 hasn't run yet: its record stays.
        assert!(updates.join(INSTALLED_FILE).exists());
    }
}

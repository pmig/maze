//! "Roll back to <previous version>" after an update that keeps crashing (PLAN §4.15,
//! crash guard).
//!
//! The core's crash guard decides *whether* the last launches failed
//! (`CoreHandle::should_offer_rollback`); this module decides whether a rollback is
//! *possible* ([`rollback_offer`]: a previous bundle was kept by the installer) and
//! performs it ([`restore_previous_bundle`]: swap it back in one step, like an
//! install). The database is not touched: restoring the pre-migration backup needs a
//! store API that doesn't exist yet.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use semver::Version;

use crate::install::{
    ARCHIVE_BUNDLE_NAME, InstallError, STAGING_PREFIX, copy_dir_all, remove_all, swap_bundles,
};
use crate::manifest::compare_versions;

/// A possible rollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackOffer {
    /// The bundle that is installed now (and crashes).
    pub installed_bundle: PathBuf,
    /// The bundle the installer kept, `<data_dir>/updates/previous/Maze.app`.
    pub previous_bundle: PathBuf,
    /// Its version, if it can be read from the bundle.
    pub previous_version: Option<Version>,
}

impl RollbackOffer {
    /// "Roll back to Maze 0.1.0" or "Roll back to the previous version".
    pub fn label(&self) -> String {
        match &self.previous_version {
            Some(version) => format!("Roll back to Maze {version}"),
            None => "Roll back to the previous version".to_owned(),
        }
    }
}

/// The previous bundle the installer keeps for `installed_bundle`.
pub fn previous_bundle_path(installed_bundle: &Path, previous_dir: &Path) -> PathBuf {
    let name = installed_bundle
        .file_name()
        .map_or_else(|| ARCHIVE_BUNDLE_NAME.into(), ToOwned::to_owned);
    previous_dir.join(name)
}

/// What to offer when the crash guard fired: a rollback if the crash guard says so
/// (`should_offer`), a previous bundle exists, and it isn't the running version or a
/// newer one.
pub fn rollback_offer(
    should_offer: bool,
    installed_bundle: &Path,
    previous_dir: &Path,
    current: &Version,
) -> Option<RollbackOffer> {
    if !should_offer {
        return None;
    }
    let previous_bundle = previous_bundle_path(installed_bundle, previous_dir);
    if !fs::symlink_metadata(&previous_bundle).is_ok_and(|meta| meta.is_dir()) {
        return None;
    }
    let previous_version = bundle_version(&previous_bundle);
    if previous_version
        .as_ref()
        .is_some_and(|version| !compare_versions(version, current).is_lt())
    {
        return None;
    }
    Some(RollbackOffer {
        installed_bundle: installed_bundle.to_path_buf(),
        previous_bundle,
        previous_version,
    })
}

/// The Maze version of a bundle: `MazeVersion` in `Contents/Info.plist` (macOS).
/// `None` if there is no such entry (Linux bundles, old bundles).
pub fn bundle_version(bundle: &Path) -> Option<Version> {
    let plist = fs::read_to_string(bundle.join("Contents/Info.plist")).ok()?;
    plist_string(&plist, "MazeVersion").and_then(|text| Version::parse(text.trim()).ok())
}

/// The `<string>` after `<key>{key}</key>` in an XML property list.
fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after_key = plist.split_once(&format!("<key>{key}</key>"))?.1;
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let (value, _) = value.split_once("</string>")?;
    Some(value.trim().to_owned())
}

/// Puts `previous_bundle` back at `installed_bundle` in one step and deletes the bundle
/// it replaces. The installed path holds a complete app at every moment. The running
/// process keeps executing the old binary until it restarts.
pub fn restore_previous_bundle(
    previous_bundle: &Path,
    installed_bundle: &Path,
) -> Result<(), InstallError> {
    let parent = installed_bundle
        .parent()
        .ok_or_else(|| InstallError::NotInBundle(installed_bundle.to_path_buf()))?;
    let staging = parent.join(format!("{STAGING_PREFIX}rollback.app"));
    remove_all(&staging);

    // Next to the installed app first, so the swap stays on one volume.
    match fs::rename(previous_bundle, &staging) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            copy_dir_all(previous_bundle, &staging).inspect_err(|_| remove_all(&staging))?;
            remove_all(previous_bundle);
        }
        Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
            return Err(InstallError::NotWritable {
                path: parent.to_path_buf(),
                source: err,
            });
        }
        Err(err) => {
            return Err(InstallError::Io {
                path: staging,
                source: err,
            });
        }
    }

    if let Err(err) = swap_bundles(&staging, installed_bundle) {
        // Keep the previous bundle for another attempt.
        if fs::rename(&staging, previous_bundle).is_err() {
            remove_all(&staging);
        }
        return Err(err);
    }
    // The bundle that crashed is now at the staging path.
    remove_all(&staging);
    tracing::info!(bundle = %installed_bundle.display(), "rolled back to the previous version");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn make_bundle(path: &Path, version: Option<&str>) {
        fs::create_dir_all(path.join("Contents/MacOS")).unwrap();
        fs::write(path.join("Contents/MacOS/mazemail"), "bin").unwrap();
        if let Some(version) = version {
            fs::write(
                path.join("Contents/Info.plist"),
                format!(
                    "<plist version=\"1.0\">\n<dict>\n  <key>CFBundleName</key>\n  \
                     <string>Maze</string>\n  <key>MazeVersion</key>\n  \
                     <string>{version}</string>\n</dict>\n</plist>\n"
                ),
            )
            .unwrap();
        }
    }

    #[test]
    fn reads_the_version_of_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = dir.path().join("Maze.app");
        make_bundle(&bundle, Some("0.1.0-alpha.1"));
        assert_eq!(bundle_version(&bundle), Some(v("0.1.0-alpha.1")));
        assert_eq!(bundle_version(&dir.path().join("Missing.app")), None);
        assert_eq!(
            plist_string("<key>MazeVersion</key><true/>", "MazeVersion"),
            None
        );
    }

    #[test]
    fn offers_a_rollback_only_when_it_is_possible() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Applications/Maze.app");
        make_bundle(&installed, Some("0.2.0"));
        let previous_dir = dir.path().join("data/updates/previous");
        let current = v("0.2.0");

        // No previous bundle.
        assert_eq!(
            rollback_offer(true, &installed, &previous_dir, &current),
            None
        );

        make_bundle(&previous_dir.join("Maze.app"), Some("0.1.0"));
        // The crash guard didn't fire.
        assert_eq!(
            rollback_offer(false, &installed, &previous_dir, &current),
            None
        );
        let offer = rollback_offer(true, &installed, &previous_dir, &current).unwrap();
        assert_eq!(offer.previous_bundle, previous_dir.join("Maze.app"));
        assert_eq!(offer.previous_version, Some(v("0.1.0")));
        assert_eq!(offer.label(), "Roll back to Maze 0.1.0");

        // The kept bundle is the running version (or newer): nothing to roll back to.
        assert_eq!(
            rollback_offer(true, &installed, &previous_dir, &v("0.1.0")),
            None
        );

        // Unknown version (Linux): offered, with a generic label.
        fs::remove_file(previous_dir.join("Maze.app/Contents/Info.plist")).unwrap();
        let offer = rollback_offer(true, &installed, &previous_dir, &current).unwrap();
        assert_eq!(offer.label(), "Roll back to the previous version");
    }

    #[test]
    fn restores_the_previous_bundle_in_one_step() {
        let dir = tempfile::tempdir().unwrap();
        let apps = dir.path().join("Applications");
        let installed = apps.join("Maze.app");
        make_bundle(&installed, Some("0.2.0"));
        let previous = dir.path().join("data/updates/previous/Maze.app");
        make_bundle(&previous, Some("0.1.0"));

        restore_previous_bundle(&previous, &installed).unwrap();
        assert_eq!(bundle_version(&installed), Some(v("0.1.0")));
        // The crashing bundle is gone, and so is the kept copy (it is installed now).
        assert!(!previous.exists());
        assert_eq!(fs::read_dir(&apps).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_restore_keeps_both_bundles() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("Applications/Maze.app");
        // Nothing installed: the swap fails.
        fs::create_dir_all(dir.path().join("Applications")).unwrap();
        let previous = dir.path().join("previous/Maze.app");
        make_bundle(&previous, Some("0.1.0"));
        assert!(restore_previous_bundle(&previous, &installed).is_err());
        assert_eq!(bundle_version(&previous), Some(v("0.1.0")));
        assert_eq!(
            fs::read_dir(dir.path().join("Applications"))
                .unwrap()
                .count(),
            0
        );
    }
}

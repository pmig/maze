//! Hot reload: watches the settings file and publishes reloaded settings.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{DebounceEventResult, Debouncer, new_debouncer};
use tokio::sync::watch;

use crate::{LoadedSettings, load_settings};

/// How long the file must be quiet before it is reloaded. Editors often write a file
/// in several steps (truncate, write, rename).
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(100);

/// Error starting a [`SettingsWatcher`].
#[derive(Debug, thiserror::Error)]
pub enum WatchError {
    /// The settings path has no parent directory or file name.
    #[error("invalid settings path {0}")]
    InvalidPath(PathBuf),
    /// The config directory couldn't be created.
    #[error("cannot create {path}: {source}")]
    CreateDir {
        /// The directory.
        path: PathBuf,
        /// The cause.
        source: std::io::Error,
    },
    /// The file system watcher couldn't be started.
    #[error("cannot watch {path}: {source}")]
    Watch {
        /// The watched directory.
        path: PathBuf,
        /// The cause.
        source: notify::Error,
    },
}

/// Watches the user's settings file. Dropping it stops watching.
///
/// The directory is watched rather than the file, so atomic saves (write to a temporary
/// file, rename over the original) and a file that doesn't exist yet both work.
pub struct SettingsWatcher {
    path: PathBuf,
    sender: watch::Sender<Arc<LoadedSettings>>,
    _debouncer: Debouncer<RecommendedWatcher>,
}

impl std::fmt::Debug for SettingsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsWatcher")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SettingsWatcher {
    /// Loads `path` and starts watching it. The receiver holds the current settings
    /// immediately and is notified after every change that alters the result
    /// (settings or diagnostics). Creates the parent directory if needed.
    pub fn start(
        path: impl Into<PathBuf>,
        debounce: Duration,
    ) -> Result<(Self, watch::Receiver<Arc<LoadedSettings>>), WatchError> {
        let path = path.into();
        let (Some(dir), Some(file_name)) = (path.parent(), path.file_name()) else {
            return Err(WatchError::InvalidPath(path));
        };
        let dir = dir.to_path_buf();
        let file_name = file_name.to_os_string();
        std::fs::create_dir_all(&dir).map_err(|source| WatchError::CreateDir {
            path: dir.clone(),
            source,
        })?;

        let (sender, receiver) = watch::channel(Arc::new(load_settings(&path)));
        let reload_path = path.clone();
        let reload_sender = sender.clone();
        let mut debouncer = new_debouncer(debounce, move |result: DebounceEventResult| {
            handle_events(result, &reload_path, &file_name, &reload_sender);
        })
        .map_err(|source| WatchError::Watch {
            path: dir.clone(),
            source,
        })?;
        debouncer
            .watcher()
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|source| WatchError::Watch {
                path: dir.clone(),
                source,
            })?;

        Ok((
            Self {
                path,
                sender,
                _debouncer: debouncer,
            },
            receiver,
        ))
    }

    /// The watched settings file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reloads the file now and publishes the result if it changed, without waiting
    /// for the file system event. For Maze's own writes ([`crate::write_setting`]),
    /// so what it just changed shows at once; the event that follows finds nothing
    /// new.
    pub fn reload(&self) {
        publish(&self.path, &self.sender);
    }
}

fn handle_events(
    result: DebounceEventResult,
    path: &Path,
    file_name: &OsString,
    sender: &watch::Sender<Arc<LoadedSettings>>,
) {
    let events = match result {
        Ok(events) => events,
        Err(err) => {
            tracing::warn!(%err, "settings watcher error");
            return;
        }
    };
    // Paths from FSEvents are canonical (`/private/var/…`), so compare file names.
    if !events
        .iter()
        .any(|event| event.path.file_name() == Some(file_name.as_os_str()))
    {
        return;
    }
    publish(path, sender);
}

/// Loads `path` and sends the result if it differs from the current settings.
///
/// A file that is ignored as a whole (a syntax error while it is being edited, say)
/// keeps the settings that were in effect, with the new diagnostics: a typo must not
/// switch everything back to the defaults. Invalid single values still fall back to
/// their defaults, key by key.
fn publish(path: &Path, sender: &watch::Sender<Arc<LoadedSettings>>) {
    let mut loaded = load_settings(path);
    for diagnostic in &loaded.diagnostics {
        if diagnostic.is_error() {
            tracing::warn!(%diagnostic, "settings");
        } else {
            tracing::info!(%diagnostic, "settings");
        }
    }
    sender.send_if_modified(|current| {
        if loaded.file_ignored() {
            loaded.settings = current.settings.clone();
        }
        if **current == loaded {
            false
        } else {
            *current = Arc::new(loaded);
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Layout;

    async fn next(receiver: &mut watch::Receiver<Arc<LoadedSettings>>) -> Arc<LoadedSettings> {
        tokio::time::timeout(Duration::from_secs(10), receiver.changed())
            .await
            .expect("no reload within 10 s")
            .unwrap();
        receiver.borrow_and_update().clone()
    }

    #[tokio::test]
    async fn reloads_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/settings.json");
        let (watcher, mut receiver) =
            SettingsWatcher::start(&path, Duration::from_millis(50)).unwrap();
        assert_eq!(watcher.path(), path);
        assert_eq!(
            receiver.borrow_and_update().settings.layout,
            Layout::FourColumn
        );

        // Creating the file.
        std::fs::write(&path, r#"{ "layout": "wide" }"#).unwrap();
        assert_eq!(next(&mut receiver).await.settings.layout, Layout::Wide);

        // An atomic save: write elsewhere, rename over the file.
        let tmp = path.with_file_name("settings.json.tmp");
        std::fs::write(&tmp, r#"{ "layout": "classic", "oops": 1 }"#).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        let loaded = next(&mut receiver).await;
        assert_eq!(loaded.settings.layout, Layout::Classic);
        assert_eq!(loaded.diagnostics.len(), 1);

        // Deleting the file goes back to the defaults.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            next(&mut receiver).await.settings.layout,
            Layout::FourColumn
        );
    }

    #[test]
    fn reload_publishes_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // A long debounce: only `reload` can deliver the change in time.
        let (watcher, mut receiver) =
            SettingsWatcher::start(&path, Duration::from_secs(60)).unwrap();
        receiver.borrow_and_update();
        std::fs::write(&path, r#"{ "layout": "wide" }"#).unwrap();
        watcher.reload();
        assert!(receiver.has_changed().unwrap());
        assert_eq!(receiver.borrow_and_update().settings.layout, Layout::Wide);
        // Nothing new: no notification.
        watcher.reload();
        assert!(!receiver.has_changed().unwrap());
    }

    #[test]
    fn syntax_errors_keep_the_last_good_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{ "layout": "wide" }"#).unwrap();
        let (watcher, mut receiver) =
            SettingsWatcher::start(&path, Duration::from_secs(60)).unwrap();
        assert_eq!(receiver.borrow_and_update().settings.layout, Layout::Wide);

        // Half typed: the diagnostics change, the settings stay.
        std::fs::write(&path, "{ \"layout\": \"wide\",\n  \"theme\": }").unwrap();
        watcher.reload();
        assert!(receiver.has_changed().unwrap());
        let loaded = receiver.borrow_and_update().clone();
        assert_eq!(loaded.settings.layout, Layout::Wide);
        assert!(loaded.file_ignored());
        assert_eq!(loaded.diagnostics[0].line, Some(2));

        // Fixed: the new settings apply.
        std::fs::write(&path, r#"{ "layout": "classic" }"#).unwrap();
        watcher.reload();
        let loaded = receiver.borrow_and_update().clone();
        assert_eq!(loaded.settings.layout, Layout::Classic);
        assert!(loaded.diagnostics.is_empty());
    }

    #[test]
    fn rejects_paths_without_parent() {
        assert!(matches!(
            SettingsWatcher::start("/", DEFAULT_DEBOUNCE),
            Err(WatchError::InvalidPath(_))
        ));
    }
}

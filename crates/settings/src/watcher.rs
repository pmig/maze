//! Hot reload: watches the settings file and publishes reloaded settings.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use notify::event::{AccessKind, AccessMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
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
    /// The thread that reloads the file couldn't be started.
    #[error("cannot start the settings reload thread: {0}")]
    Thread(std::io::Error),
}

/// Watches the user's settings file. Dropping it stops watching.
///
/// The directory is watched rather than the file, so atomic saves (write to a temporary
/// file, rename over the original) and a file that doesn't exist yet both work.
pub struct SettingsWatcher {
    path: PathBuf,
    sender: watch::Sender<Arc<LoadedSettings>>,
    /// Dropping it ends the reload thread too.
    _watcher: RecommendedWatcher,
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

        let loaded = load_settings(&path);
        log_diagnostics(&loaded);
        let (sender, receiver) = watch::channel(Arc::new(loaded));

        let (changes, changed) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            match result {
                // Fails only once the reload thread is gone, and then nobody listens.
                Ok(event) if may_change(&event, &file_name) => _ = changes.send(()),
                Ok(_) => {}
                Err(err) => tracing::warn!(%err, "settings watcher error"),
            }
        })
        .map_err(|source| WatchError::Watch {
            path: dir.clone(),
            source,
        })?;
        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .map_err(|source| WatchError::Watch {
                path: dir.clone(),
                source,
            })?;

        let reload_path = path.clone();
        let reload_sender = sender.clone();
        std::thread::Builder::new()
            .name("settings-reload".to_owned())
            .spawn(move || {
                reload_when_quiet(&changed, debounce, || {
                    publish(&reload_path, &reload_sender);
                });
            })
            .map_err(WatchError::Thread)?;

        Ok((
            Self {
                path,
                sender,
                _watcher: watcher,
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

/// Whether `event` may have changed the file named `file_name`. Opening and reading
/// it doesn't: inotify reports every open, reloading opens the file, and counting
/// those would reload the settings over and over. Closing it after writing does.
fn may_change(event: &Event, file_name: &OsStr) -> bool {
    let reads_only = matches!(
        event.kind,
        EventKind::Access(access) if access != AccessKind::Close(AccessMode::Write)
    );
    // Paths from FSEvents are canonical (`/private/var/…`), so compare file names.
    !reads_only
        && event
            .paths
            .iter()
            .any(|path| path.file_name() == Some(file_name))
}

/// Calls `reload` once the changes have stopped for `quiet`, until the watcher (the
/// sending side) is dropped.
fn reload_when_quiet(changed: &mpsc::Receiver<()>, quiet: Duration, reload: impl Fn()) {
    while changed.recv().is_ok() {
        loop {
            match changed.recv_timeout(quiet) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        reload();
    }
}

/// Logs the diagnostics of a load: errors as warnings, the rest as info.
fn log_diagnostics(loaded: &LoadedSettings) {
    for diagnostic in &loaded.diagnostics {
        if diagnostic.is_error() {
            tracing::warn!(%diagnostic, "settings");
        } else {
            tracing::info!(%diagnostic, "settings");
        }
    }
}

/// Loads `path` and sends the result if it differs from the current settings. The
/// diagnostics are logged when they differ from the last load's.
///
/// A file that is ignored as a whole (a syntax error while it is being edited, say)
/// keeps the settings that were in effect, with the new diagnostics: a typo must not
/// switch everything back to the defaults. Invalid single values still fall back to
/// their defaults, key by key.
fn publish(path: &Path, sender: &watch::Sender<Arc<LoadedSettings>>) {
    let mut loaded = load_settings(path);
    if sender.borrow().diagnostics != loaded.diagnostics {
        log_diagnostics(&loaded);
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
    fn reading_the_file_is_no_change() {
        use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};

        let file = OsStr::new("settings.json");
        let on = |kind, name: &str| Event::new(kind).add_path(PathBuf::from("/c").join(name));
        let changes = [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Remove(RemoveKind::File),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Any,
        ];
        for kind in changes {
            assert!(may_change(&on(kind, "settings.json"), file), "{kind:?}");
            assert!(!may_change(&on(kind, "keymap.json"), file), "{kind:?}");
        }
        let reads = [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
        ];
        for kind in reads {
            assert!(!may_change(&on(kind, "settings.json"), file), "{kind:?}");
        }
        // A rename over the file names both paths.
        let rename = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(PathBuf::from("/c/settings.json.tmp"))
            .add_path(PathBuf::from("/c/settings.json"));
        assert!(may_change(&rename, file));
    }

    #[test]
    fn reloads_once_when_the_changes_stop() {
        let (changes, changed) = mpsc::channel();
        let reloads = std::sync::atomic::AtomicUsize::new(0);
        for _ in 0..3 {
            changes.send(()).unwrap();
        }
        drop(changes);
        reload_when_quiet(&changed, Duration::from_millis(10), || {
            reloads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        // The sender went away before the quiet time was up: no reload, just the end.
        assert_eq!(reloads.into_inner(), 0);

        let (changes, changed) = mpsc::channel();
        let reloads = std::sync::atomic::AtomicUsize::new(0);
        let writer = std::thread::spawn(move || {
            for _ in 0..3 {
                changes.send(()).unwrap();
            }
            std::thread::sleep(Duration::from_millis(200));
        });
        reload_when_quiet(&changed, Duration::from_millis(50), || {
            reloads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        writer.join().unwrap();
        assert_eq!(reloads.into_inner(), 1);
    }

    #[test]
    fn rejects_paths_without_parent() {
        assert!(matches!(
            SettingsWatcher::start("/", DEFAULT_DEBOUNCE),
            Err(WatchError::InvalidPath(_))
        ));
    }
}

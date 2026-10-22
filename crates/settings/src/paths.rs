//! Where Maze keeps its files (CONCEPT §5.4, PLAN §4.11).
//!
//! | What        | macOS                                        | Linux                          |
//! | ----------- | -------------------------------------------- | ------------------------------ |
//! | data dir    | `~/Library/Application Support/mazemail`     | `$XDG_DATA_HOME/mazemail`      |
//! | config dir  | `$XDG_CONFIG_HOME/mazemail` or `~/.config/mazemail` (both, like Zed)          |
//!
//! Dev builds use `mazemail-dev` instead of `mazemail` so they never touch the real
//! store. `MAZEMAIL_DATA_DIR` and `MAZEMAIL_CONFIG_DIR` override the directories
//! completely (tests, development, portable setups).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use release_channel::ReleaseChannel;

/// Overrides the data directory.
pub const DATA_DIR_ENV: &str = "MAZEMAIL_DATA_DIR";
/// Overrides the config directory.
pub const CONFIG_DIR_ENV: &str = "MAZEMAIL_CONFIG_DIR";

/// The socket's file name in the data directory.
pub const SOCKET_FILE_NAME: &str = "mazemail.sock";

/// Capacity of `sockaddr_un.sun_path` in bytes, including the terminating NUL.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub const SUN_PATH_CAPACITY: usize = 104;
/// Capacity of `sockaddr_un.sun_path` in bytes, including the terminating NUL.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
pub const SUN_PATH_CAPACITY: usize = 108;

/// Error when the directories can't be determined.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    /// Neither an override nor a home directory is available.
    #[error("cannot determine the home directory (set HOME or {DATA_DIR_ENV})")]
    NoHomeDir,
    /// An override is set to a relative path.
    #[error("{name} must be an absolute path, got {value:?}")]
    RelativeOverride {
        /// The environment variable.
        name: &'static str,
        /// Its value.
        value: PathBuf,
    },
}

/// The directories of one Maze installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    data_dir: PathBuf,
    config_dir: PathBuf,
}

impl Paths {
    /// The directories for the running build's channel, from the process environment.
    pub fn current() -> Result<Self, PathsError> {
        Self::for_channel(ReleaseChannel::current())
    }

    /// The directories for `channel`, from the process environment.
    pub fn for_channel(channel: ReleaseChannel) -> Result<Self, PathsError> {
        Self::resolve(channel, |name| std::env::var_os(name), std::env::home_dir())
    }

    /// Uses the given directories as they are.
    pub fn from_dirs(data_dir: impl Into<PathBuf>, config_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            config_dir: config_dir.into(),
        }
    }

    /// Resolves the directories from an environment lookup and a home directory.
    pub fn resolve(
        channel: ReleaseChannel,
        env: impl Fn(&str) -> Option<OsString>,
        home: Option<PathBuf>,
    ) -> Result<Self, PathsError> {
        let name = channel.dir_name();
        let home = home.filter(|home| home.is_absolute());
        // Per the XDG spec, relative values are invalid and ignored.
        let xdg = |var: &str| {
            env(var)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };

        let data_dir = match override_dir(&env, DATA_DIR_ENV)? {
            Some(dir) => dir,
            None => {
                let home = home.as_ref().ok_or(PathsError::NoHomeDir)?;
                if cfg!(target_os = "macos") {
                    home.join("Library/Application Support").join(name)
                } else {
                    xdg("XDG_DATA_HOME")
                        .unwrap_or_else(|| home.join(".local/share"))
                        .join(name)
                }
            }
        };

        let config_dir = match override_dir(&env, CONFIG_DIR_ENV)? {
            Some(dir) => dir,
            None => match xdg("XDG_CONFIG_HOME") {
                Some(config_home) => config_home.join(name),
                None => home
                    .ok_or(PathsError::NoHomeDir)?
                    .join(".config")
                    .join(name),
            },
        };

        Ok(Self {
            data_dir,
            config_dir,
        })
    }

    /// The data directory (`mail/`, `agents/`, `logs/`, the socket, …).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The config directory (`settings.json`, `keymap.json`, `themes/`).
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// The user's settings file.
    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.json")
    }

    /// The user's keymap file.
    pub fn keymap_file(&self) -> PathBuf {
        self.config_dir.join("keymap.json")
    }

    /// User themes.
    pub fn themes_dir(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    /// Everything with mail content (encrypted).
    pub fn mail_dir(&self) -> PathBuf {
        self.data_dir.join("mail")
    }

    /// Log files.
    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    /// Downloaded updates and the previous app bundle.
    pub fn updates_dir(&self) -> PathBuf {
        self.data_dir.join("updates")
    }

    /// Generated JSON Schemas for editor autocompletion.
    pub fn schemas_dir(&self) -> PathBuf {
        self.data_dir.join("schemas")
    }

    /// The generated settings schema.
    pub fn settings_schema_file(&self) -> PathBuf {
        self.schemas_dir().join("settings.json")
    }

    /// Where the server records the socket path when it had to use the fallback.
    pub fn socket_path_file(&self) -> PathBuf {
        self.data_dir.join("socket-path")
    }

    /// The local API socket for this data directory, see [`socket_path`].
    pub fn socket_path(&self) -> SocketPath {
        socket_path(&self.data_dir, &std::env::temp_dir())
    }
}

fn override_dir(
    env: &impl Fn(&str) -> Option<OsString>,
    name: &'static str,
) -> Result<Option<PathBuf>, PathsError> {
    match env(name).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(Some(path))
            } else {
                Err(PathsError::RelativeOverride { name, value: path })
            }
        }
    }
}

/// Where the local API socket lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketPath {
    /// `<data_dir>/mazemail.sock` fits into `sun_path`.
    InDataDir(PathBuf),
    /// The data directory path is too long for a Unix socket. The server creates `dir`
    /// with mode 0700, binds `path` in it and records `path` in
    /// [`Paths::socket_path_file`] for the CLI and the sandbox profile.
    Fallback {
        /// The short private directory.
        dir: PathBuf,
        /// The socket inside it.
        path: PathBuf,
    },
}

impl SocketPath {
    /// The socket path to bind or connect to.
    pub fn path(&self) -> &Path {
        match self {
            Self::InDataDir(path) | Self::Fallback { path, .. } => path,
        }
    }
}

/// Computes the socket path for `data_dir`, falling back to a short directory under
/// `tmp_dir` (or `/tmp`) when `<data_dir>/mazemail.sock` exceeds
/// [`SUN_PATH_CAPACITY`] (PLAN §4.11).
///
/// The fallback directory name contains a stable hash of `data_dir`, so dev and release
/// builds, or several test data dirs, never share a socket.
pub fn socket_path(data_dir: &Path, tmp_dir: &Path) -> SocketPath {
    let direct = data_dir.join(SOCKET_FILE_NAME);
    if fits_sun_path(&direct) {
        return SocketPath::InDataDir(direct);
    }
    let dir_name = format!(
        "mazemail-{:016x}",
        fnv1a(data_dir.as_os_str().as_encoded_bytes())
    );
    let mut dir = tmp_dir.join(&dir_name);
    if !fits_sun_path(&dir.join(SOCKET_FILE_NAME)) {
        dir = Path::new("/tmp").join(dir_name);
    }
    let path = dir.join(SOCKET_FILE_NAME);
    SocketPath::Fallback { dir, path }
}

fn fits_sun_path(path: &Path) -> bool {
    // One byte is needed for the terminating NUL.
    path.as_os_str().as_encoded_bytes().len() < SUN_PATH_CAPACITY
}

/// 64-bit FNV-1a: tiny, deterministic across Rust versions (unlike `DefaultHasher`).
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn resolve(
        channel: ReleaseChannel,
        vars: &[(&str, &str)],
        home: Option<&str>,
    ) -> Result<Paths, PathsError> {
        let vars: HashMap<String, OsString> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        Paths::resolve(
            channel,
            |name| vars.get(name).cloned(),
            home.map(PathBuf::from),
        )
    }

    #[test]
    fn defaults_per_platform_and_channel() {
        let paths = resolve(ReleaseChannel::Stable, &[], Some("/home/anna")).unwrap();
        let dev = resolve(ReleaseChannel::Dev, &[], Some("/home/anna")).unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(
                paths.data_dir(),
                Path::new("/home/anna/Library/Application Support/mazemail")
            );
            assert_eq!(
                dev.data_dir(),
                Path::new("/home/anna/Library/Application Support/mazemail-dev")
            );
        } else {
            assert_eq!(
                paths.data_dir(),
                Path::new("/home/anna/.local/share/mazemail")
            );
            assert_eq!(
                dev.data_dir(),
                Path::new("/home/anna/.local/share/mazemail-dev")
            );
        }
        assert_eq!(paths.config_dir(), Path::new("/home/anna/.config/mazemail"));
        assert_eq!(
            dev.config_dir(),
            Path::new("/home/anna/.config/mazemail-dev")
        );
        assert_eq!(
            paths.settings_file(),
            Path::new("/home/anna/.config/mazemail/settings.json")
        );
        assert_eq!(paths.logs_dir(), paths.data_dir().join("logs"));
        assert_eq!(
            paths.settings_schema_file(),
            paths.data_dir().join("schemas/settings.json")
        );
    }

    #[test]
    fn xdg_variables() {
        let paths = resolve(
            ReleaseChannel::Preview,
            &[
                ("XDG_CONFIG_HOME", "/xdg/config"),
                ("XDG_DATA_HOME", "/xdg/data"),
            ],
            Some("/home/anna"),
        )
        .unwrap();
        assert_eq!(paths.config_dir(), Path::new("/xdg/config/mazemail"));
        if cfg!(target_os = "macos") {
            // macOS keeps data in Application Support regardless of XDG.
            assert!(paths.data_dir().starts_with("/home/anna/Library"));
        } else {
            assert_eq!(paths.data_dir(), Path::new("/xdg/data/mazemail"));
        }

        // Relative XDG values are invalid per spec and ignored.
        let paths = resolve(
            ReleaseChannel::Stable,
            &[("XDG_CONFIG_HOME", "config")],
            Some("/home/anna"),
        )
        .unwrap();
        assert_eq!(paths.config_dir(), Path::new("/home/anna/.config/mazemail"));
    }

    #[test]
    fn overrides_win() {
        let paths = resolve(
            ReleaseChannel::Dev,
            &[(DATA_DIR_ENV, "/tmp/d"), (CONFIG_DIR_ENV, "/tmp/c")],
            None,
        )
        .unwrap();
        assert_eq!(paths, Paths::from_dirs("/tmp/d", "/tmp/c"));

        let err = resolve(ReleaseChannel::Dev, &[(DATA_DIR_ENV, "rel")], None).unwrap_err();
        assert!(matches!(err, PathsError::RelativeOverride { .. }));
        let err = resolve(ReleaseChannel::Dev, &[], None).unwrap_err();
        assert!(matches!(err, PathsError::NoHomeDir));
        // An empty override counts as unset.
        let paths = resolve(ReleaseChannel::Dev, &[(DATA_DIR_ENV, "")], Some("/h")).unwrap();
        assert!(paths.data_dir().ends_with("mazemail-dev"));
    }

    #[test]
    fn short_socket_path_stays_in_data_dir() {
        let data = Path::new("/Users/anna/Library/Application Support/mazemail");
        let socket = socket_path(data, Path::new("/tmp"));
        assert_eq!(socket, SocketPath::InDataDir(data.join("mazemail.sock")));
    }

    #[test]
    fn long_socket_path_falls_back_to_tmp() {
        let long_user = "a".repeat(60);
        let data = PathBuf::from(format!(
            "/Users/{long_user}/Library/Application Support/mazemail"
        ));
        assert!(data.join(SOCKET_FILE_NAME).as_os_str().len() >= SUN_PATH_CAPACITY);
        let tmp = Path::new("/var/folders/xy/abcdefghijklmnopqrstuvwxyz012345/T");
        let socket = socket_path(&data, tmp);
        let SocketPath::Fallback { dir, path } = &socket else {
            panic!("expected fallback, got {socket:?}");
        };
        assert!(dir.starts_with(tmp));
        assert_eq!(path, &dir.join(SOCKET_FILE_NAME));
        assert!(path.as_os_str().len() < SUN_PATH_CAPACITY);
        // Stable for the same data dir, different for another one.
        assert_eq!(socket_path(&data, tmp), socket);
        assert_ne!(socket_path(&data.join("x"), tmp), socket);

        // A $TMPDIR that is itself too long ends up in /tmp.
        let long_tmp = PathBuf::from(format!("/{}", "t".repeat(120)));
        let socket = socket_path(&data, &long_tmp);
        assert!(
            socket
                .path()
                .to_string_lossy()
                .starts_with("/tmp/mazemail-")
        );
    }

    #[test]
    fn exact_boundary() {
        // `/` + name + `/mazemail.sock` with exactly CAPACITY - 1 bytes still fits.
        let budget = SUN_PATH_CAPACITY - 1 - "/".len() - "/mazemail.sock".len();
        let data = PathBuf::from(format!("/{}", "d".repeat(budget)));
        assert!(matches!(
            socket_path(&data, Path::new("/tmp")),
            SocketPath::InDataDir(_)
        ));
        let data = PathBuf::from(format!("/{}", "d".repeat(budget + 1)));
        assert!(matches!(
            socket_path(&data, Path::new("/tmp")),
            SocketPath::Fallback { .. }
        ));
    }
}

//! Release channel, version and commit of this Maze build.
//!
//! Everything here is fixed at build time (PLAN §4.15, §4.16):
//!
//! - the **channel** comes from `crates/mazemail/RELEASE_CHANNEL` (`dev` in the
//!   repository; the release workflow overwrites it with `preview` or `stable`), like
//!   Zed's `crates/zed/RELEASE_CHANNEL`. The build script reads it, so a missing or
//!   empty file means `dev`;
//! - the **version** comes from `version.txt`, which release-please maintains;
//! - the **commit** comes from the build script (`MAZEMAIL_COMMIT_SHA` or
//!   `git rev-parse --short HEAD`, else `unknown`).
//!
//! Only `dev` builds are a separate app (own bundle ID, app name and data directory),
//! because preview and stable share one data store; for those the update channel is a
//! setting, not a property of the bundle.
//!
//! This crate is layer L0: no IO, no async.

use std::fmt;
use std::str::FromStr;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

pub use semver::Version;

/// The channel name the build script read from `crates/mazemail/RELEASE_CHANNEL`.
const CHANNEL_NAME: &str = env!("MAZEMAIL_RELEASE_CHANNEL");

/// The version embedded from `version.txt`, trimmed. The build script has checked that
/// it is valid semver.
pub const VERSION: &str = include_str!("../../../version.txt").trim_ascii();

/// The short git commit this build was made from, or `unknown`.
pub const COMMIT_SHA: &str = env!("MAZEMAIL_COMMIT_SHA");

/// The release channel of this build. An invalid `RELEASE_CHANNEL` file is a build
/// error.
pub const RELEASE_CHANNEL: ReleaseChannel = match ReleaseChannel::from_name(CHANNEL_NAME) {
    Some(channel) => channel,
    None => panic!("RELEASE_CHANNEL must contain dev, preview or stable"),
};

/// The release channel of a build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseChannel {
    /// Local builds. A separate app (`Maze Dev`) with its own data directory; never
    /// updates itself.
    Dev,
    /// Prereleases (alpha, beta, rc).
    Preview,
    /// Stable releases.
    Stable,
}

impl ReleaseChannel {
    /// All channels, from least to most stable.
    pub const ALL: [ReleaseChannel; 3] = [Self::Dev, Self::Preview, Self::Stable];

    /// The channel of this build (same as [`RELEASE_CHANNEL`]).
    pub const fn current() -> Self {
        RELEASE_CHANNEL
    }

    /// Parses a lowercase channel name (`dev`, `preview`, `stable`); usable in `const`
    /// contexts.
    pub const fn from_name(name: &str) -> Option<Self> {
        match name.as_bytes() {
            b"dev" => Some(Self::Dev),
            b"preview" => Some(Self::Preview),
            b"stable" => Some(Self::Stable),
            _ => None,
        }
    }

    /// The technical name, as used in the `RELEASE_CHANNEL` file, manifest URLs and
    /// settings.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Preview => "preview",
            Self::Stable => "stable",
        }
    }

    /// A human-readable name ("Dev", "Preview", "Stable").
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Dev => "Dev",
            Self::Preview => "Preview",
            Self::Stable => "Stable",
        }
    }

    /// The application name people see: `Maze`, or `Maze Dev` for dev builds.
    pub const fn app_name(self) -> &'static str {
        match self {
            Self::Dev => "Maze Dev",
            Self::Preview | Self::Stable => "Maze",
        }
    }

    /// The macOS bundle ID (also the Flatpak app ID).
    pub const fn bundle_id(self) -> &'static str {
        match self {
            Self::Dev => "dev.mazemail.maze-dev",
            Self::Preview | Self::Stable => "dev.mazemail.maze",
        }
    }

    /// The directory name used for the data and config directories: `mazemail`, or
    /// `mazemail-dev` so local builds never touch the real store.
    pub const fn dir_name(self) -> &'static str {
        match self {
            Self::Dev => "mazemail-dev",
            Self::Preview | Self::Stable => "mazemail",
        }
    }

    /// Whether builds of this channel look for updates. Dev builds never do.
    pub const fn receives_updates(self) -> bool {
        !matches!(self, Self::Dev)
    }
}

impl fmt::Display for ReleaseChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Error returned when parsing an unknown channel name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown release channel {0:?} (expected dev, preview or stable)")]
pub struct ParseReleaseChannelError(String);

impl FromStr for ReleaseChannel {
    type Err = ParseReleaseChannelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s.trim()).ok_or_else(|| ParseReleaseChannelError(s.to_owned()))
    }
}

/// Version, channel and commit of a build.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AppVersion {
    /// The semantic version from `version.txt`, e.g. `0.2.0-alpha.3`.
    pub semver: Version,
    /// The release channel.
    pub channel: ReleaseChannel,
    /// The short git commit, or `unknown`.
    pub commit: String,
}

static CURRENT: LazyLock<AppVersion> = LazyLock::new(|| AppVersion {
    semver: parse_embedded_version(),
    channel: RELEASE_CHANNEL,
    commit: COMMIT_SHA.to_owned(),
});

fn parse_embedded_version() -> Version {
    match Version::parse(VERSION) {
        Ok(version) => version,
        // The build script rejects a `version.txt` that isn't semver, so this is a
        // build invariant, not a runtime failure.
        Err(err) => unreachable!("version.txt was validated at build time: {err}"),
    }
}

impl AppVersion {
    /// The version of the running build.
    pub fn current() -> &'static AppVersion {
        &CURRENT
    }

    /// The `User-Agent` for every HTTP request Maze itself makes:
    /// `mazemail/<version>`, and nothing else (PLAN §4.15).
    pub fn user_agent(&self) -> String {
        format!("mazemail/{}", self.semver)
    }
}

impl fmt::Display for AppVersion {
    /// `0.2.0-alpha.3 (preview, 9f2c1e7)`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({}, {})", self.semver, self.channel, self.commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_values_are_consistent() {
        let current = AppVersion::current();
        assert_eq!(current.semver.to_string(), VERSION);
        assert_eq!(current.channel, RELEASE_CHANNEL);
        assert!(!current.commit.is_empty());
        assert_eq!(ReleaseChannel::current(), RELEASE_CHANNEL);
    }

    #[test]
    fn names_round_trip() {
        for channel in ReleaseChannel::ALL {
            assert_eq!(channel.name().parse::<ReleaseChannel>(), Ok(channel));
            assert_eq!(channel.to_string(), channel.name());
            let json = serde_json::to_string(&channel).unwrap();
            assert_eq!(json, format!("\"{}\"", channel.name()));
            assert_eq!(
                serde_json::from_str::<ReleaseChannel>(&json).unwrap(),
                channel
            );
        }
        assert_eq!(
            " stable\n".parse::<ReleaseChannel>(),
            Ok(ReleaseChannel::Stable)
        );
        assert!("nightly".parse::<ReleaseChannel>().is_err());
        assert!("Stable".parse::<ReleaseChannel>().is_err());
    }

    #[test]
    fn only_dev_is_a_separate_app() {
        use ReleaseChannel::*;
        assert_eq!(Dev.bundle_id(), "dev.mazemail.maze-dev");
        assert_eq!(Preview.bundle_id(), "dev.mazemail.maze");
        assert_eq!(Stable.bundle_id(), "dev.mazemail.maze");
        assert_eq!(Dev.app_name(), "Maze Dev");
        assert_eq!(Preview.app_name(), "Maze");
        assert_eq!(Stable.app_name(), "Maze");
        assert_eq!(Dev.dir_name(), "mazemail-dev");
        assert_eq!(Preview.dir_name(), "mazemail");
        assert_eq!(Stable.dir_name(), "mazemail");
        assert!(!Dev.receives_updates());
        assert!(Preview.receives_updates());
        assert!(Stable.receives_updates());
    }

    #[test]
    fn display_and_user_agent() {
        let version = AppVersion {
            semver: Version::parse("0.2.0-alpha.3").unwrap(),
            channel: ReleaseChannel::Preview,
            commit: "9f2c1e7".to_owned(),
        };
        assert_eq!(version.to_string(), "0.2.0-alpha.3 (preview, 9f2c1e7)");
        assert_eq!(version.user_agent(), "mazemail/0.2.0-alpha.3");
    }
}

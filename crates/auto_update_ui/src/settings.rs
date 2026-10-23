//! The update settings (PLAN §4.15) as the updater uses them.

use auto_update::manifest::manifest_url;
use auto_update::{InstallMode, Platform};
use release_channel::ReleaseChannel;
use settings::{AutoUpdateInstall, Settings};

/// `auto_update`, `update_channel` and `auto_update_install`, resolved for the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateSettings {
    /// Check in the background (`"auto_update": true`). `false`: only when asked
    /// ("Maze: Check for Updates").
    pub auto_check: bool,
    /// The channel to follow: the `update_channel` setting, or the build's channel.
    pub channel: ReleaseChannel,
    /// Install in the background or only tell (`"auto_update_install"`).
    pub install_mode: InstallMode,
}

impl UpdateSettings {
    /// The update settings of `settings` for a build of channel `build`.
    pub fn from_settings(settings: &Settings, build: ReleaseChannel) -> Self {
        Self {
            auto_check: settings.auto_update,
            channel: settings.effective_update_channel(build),
            install_mode: match settings.auto_update_install {
                AutoUpdateInstall::Background => InstallMode::Background,
                AutoUpdateInstall::NotifyOnly => InstallMode::NotifyOnly,
            },
        }
    }

    /// The manifest these settings make the updater fetch from `base`.
    pub fn manifest_url(&self, base: &str, platform: &Platform) -> String {
        manifest_url(base, self.channel, platform)
    }
}

#[cfg(test)]
mod tests {
    use auto_update::manifest::DEFAULT_MANIFEST_BASE_URL;
    use pretty_assertions::assert_eq;
    use settings::parse_settings;

    use super::*;

    fn resolve(json: &str, build: ReleaseChannel) -> UpdateSettings {
        let loaded = parse_settings(Some(json));
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        UpdateSettings::from_settings(&loaded.settings, build)
    }

    #[test]
    fn defaults_follow_the_build() {
        assert_eq!(
            resolve("{}", ReleaseChannel::Preview),
            UpdateSettings {
                auto_check: true,
                channel: ReleaseChannel::Preview,
                install_mode: InstallMode::Background,
            }
        );
        assert_eq!(
            resolve("{}", ReleaseChannel::Stable).channel,
            ReleaseChannel::Stable
        );
    }

    #[test]
    fn reads_the_three_settings() {
        let settings = resolve(
            r#"{ "auto_update": false, "update_channel": "preview",
                 "auto_update_install": "notify_only" }"#,
            ReleaseChannel::Stable,
        );
        assert_eq!(
            settings,
            UpdateSettings {
                auto_check: false,
                channel: ReleaseChannel::Preview,
                install_mode: InstallMode::NotifyOnly,
            }
        );
        // Dev builds stay on dev whatever the setting says (and never update).
        assert_eq!(
            resolve(r#"{ "update_channel": "stable" }"#, ReleaseChannel::Dev).channel,
            ReleaseChannel::Dev
        );
    }

    #[test]
    fn the_channel_picks_the_manifest() {
        let platform = Platform::new("macos", "aarch64");
        let stable = resolve("{}", ReleaseChannel::Stable);
        assert_eq!(
            stable.manifest_url(DEFAULT_MANIFEST_BASE_URL, &platform),
            "https://releases.mazemail.dev/stable/macos-aarch64.json"
        );
        let preview = resolve(r#"{ "update_channel": "preview" }"#, ReleaseChannel::Stable);
        assert_eq!(
            preview.manifest_url("http://127.0.0.1:8000/", &platform),
            "http://127.0.0.1:8000/preview/macos-aarch64.json"
        );
    }
}

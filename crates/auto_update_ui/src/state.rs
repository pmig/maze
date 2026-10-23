//! What the title bar shows for an updater status, as a pure function so every case is
//! testable without a window.

use auto_update::{UpdateError, UpdateStatus};
use mazemail_types::PRODUCT_NAME;
use semver::Version;

/// The title bar element for the updater.
#[derive(Debug, Clone, PartialEq)]
pub enum TitleBarState {
    /// Nothing (idle, background checks, disabled updates, network errors of
    /// background checks).
    Hidden,
    /// A check the user asked for is running.
    Checking,
    /// A small progress indicator.
    Downloading {
        /// The version being downloaded.
        version: Version,
        /// `0.0..=1.0`, `None` before the first bytes.
        progress: Option<f32>,
    },
    /// Installing (a spinner; never interrupted).
    Installing {
        /// The version being installed.
        version: Version,
    },
    /// The "Restart to update" button.
    Restart {
        /// The installed version.
        version: Version,
    },
    /// The "Update to x.y.z" button (`notify_only`, or Maze can't update itself in
    /// place and offers the `.dmg`).
    Available {
        /// The offered version.
        version: Version,
        /// `true`: clicking opens the `.dmg` instead of installing.
        manual: bool,
    },
    /// A small warning icon; the error is in its tooltip.
    Error {
        /// What went wrong.
        message: String,
    },
}

/// What else decides the title bar besides the status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateContext {
    /// The running or last check was started by the user.
    pub manual_check: bool,
    /// Maze can't install in place (translocated, or the app folder isn't writable).
    pub manual_install: bool,
}

/// The title bar element for `status`.
pub fn title_bar_state(status: &UpdateStatus, context: StateContext) -> TitleBarState {
    match status {
        UpdateStatus::Idle => TitleBarState::Hidden,
        UpdateStatus::Checking if context.manual_check => TitleBarState::Checking,
        UpdateStatus::Checking => TitleBarState::Hidden,
        UpdateStatus::Available { version } => TitleBarState::Available {
            version: version.clone(),
            manual: context.manual_install,
        },
        UpdateStatus::Downloading { version, progress } => TitleBarState::Downloading {
            version: version.clone(),
            progress: *progress,
        },
        UpdateStatus::Installing { version } => TitleBarState::Installing {
            version: version.clone(),
        },
        UpdateStatus::Updated { version } => TitleBarState::Restart {
            version: version.clone(),
        },
        UpdateStatus::Errored { error } => match &**error {
            // Explained by "Maze: Check for Updates", never in the title bar.
            UpdateError::Disabled(_) => TitleBarState::Hidden,
            // Offline laptops shouldn't wear a warning sign; the next check retries.
            UpdateError::Http(_) if !context.manual_check => TitleBarState::Hidden,
            error => TitleBarState::Error {
                message: format!("Update failed: {error}"),
            },
        },
    }
}

impl TitleBarState {
    /// The button or indicator label, if it has one.
    pub fn label(&self) -> Option<String> {
        match self {
            Self::Hidden | Self::Error { .. } => None,
            Self::Checking => Some("Checking for updates…".to_owned()),
            Self::Downloading { .. } | Self::Installing { .. } => None,
            Self::Restart { .. } => Some("Restart to update".to_owned()),
            Self::Available { version, .. } => Some(format!("Update to {version}")),
        }
    }

    /// The tooltip.
    pub fn tooltip(&self) -> Option<String> {
        match self {
            Self::Hidden | Self::Checking => None,
            Self::Downloading {
                version,
                progress: Some(progress),
            } => Some(format!(
                "Downloading {PRODUCT_NAME} {version} ({:.0} %)",
                progress * 100.0
            )),
            Self::Downloading { version, .. } => {
                Some(format!("Downloading {PRODUCT_NAME} {version}…"))
            }
            Self::Installing { version } => Some(format!("Installing {PRODUCT_NAME} {version}…")),
            Self::Restart { version } => Some(format!(
                "{PRODUCT_NAME} {version} is installed. Restart {PRODUCT_NAME} to use it."
            )),
            Self::Available {
                version,
                manual: false,
            } => Some(format!("Download and install {PRODUCT_NAME} {version}")),
            Self::Available {
                version,
                manual: true,
            } => Some(format!(
                "Download {PRODUCT_NAME} {version}. {}",
                crate::MOVE_TO_APPLICATIONS
            )),
            Self::Error { message } => Some(format!("{message} (click to dismiss)")),
        }
    }

    /// A short name for tests and debug selectors.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Checking => "checking",
            Self::Downloading { .. } => "downloading",
            Self::Installing { .. } => "installing",
            Self::Restart { .. } => "restart",
            Self::Available { .. } => "available",
            Self::Error { .. } => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use auto_update::{InstallError, ManifestError};
    use pretty_assertions::assert_eq;

    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    fn background() -> StateContext {
        StateContext::default()
    }

    fn errored(error: UpdateError) -> UpdateStatus {
        UpdateStatus::Errored {
            error: Arc::new(error),
        }
    }

    #[test]
    fn idle_and_background_checks_show_nothing() {
        assert_eq!(
            title_bar_state(&UpdateStatus::Idle, background()),
            TitleBarState::Hidden
        );
        assert_eq!(
            title_bar_state(&UpdateStatus::Checking, background()),
            TitleBarState::Hidden
        );
        let manual = StateContext {
            manual_check: true,
            ..StateContext::default()
        };
        let checking = title_bar_state(&UpdateStatus::Checking, manual);
        assert_eq!(checking, TitleBarState::Checking);
        assert_eq!(checking.label().as_deref(), Some("Checking for updates…"));
    }

    #[test]
    fn downloads_show_progress() {
        let state = title_bar_state(
            &UpdateStatus::Downloading {
                version: v("0.1.0-alpha.2"),
                progress: Some(0.456),
            },
            background(),
        );
        assert_eq!(state.kind(), "downloading");
        assert_eq!(state.label(), None);
        assert_eq!(
            state.tooltip().as_deref(),
            Some("Downloading Maze 0.1.0-alpha.2 (46 %)")
        );
        let starting = title_bar_state(
            &UpdateStatus::Downloading {
                version: v("0.2.0"),
                progress: None,
            },
            background(),
        );
        assert_eq!(
            starting.tooltip().as_deref(),
            Some("Downloading Maze 0.2.0…")
        );
        let installing = title_bar_state(
            &UpdateStatus::Installing {
                version: v("0.2.0"),
            },
            background(),
        );
        assert_eq!(
            installing.tooltip().as_deref(),
            Some("Installing Maze 0.2.0…")
        );
    }

    #[test]
    fn an_installed_update_offers_the_restart() {
        let state = title_bar_state(
            &UpdateStatus::Updated {
                version: v("0.1.0-alpha.2"),
            },
            background(),
        );
        assert_eq!(
            state,
            TitleBarState::Restart {
                version: v("0.1.0-alpha.2")
            }
        );
        assert_eq!(state.label().as_deref(), Some("Restart to update"));
        assert_eq!(
            state.tooltip().as_deref(),
            Some("Maze 0.1.0-alpha.2 is installed. Restart Maze to use it.")
        );
    }

    #[test]
    fn available_updates_offer_an_install_or_the_dmg() {
        let status = UpdateStatus::Available {
            version: v("0.3.0"),
        };
        let state = title_bar_state(&status, background());
        assert_eq!(state.label().as_deref(), Some("Update to 0.3.0"));
        assert_eq!(
            state.tooltip().as_deref(),
            Some("Download and install Maze 0.3.0")
        );
        let manual = title_bar_state(
            &status,
            StateContext {
                manual_install: true,
                ..StateContext::default()
            },
        );
        assert_eq!(
            manual,
            TitleBarState::Available {
                version: v("0.3.0"),
                manual: true
            }
        );
        assert_eq!(
            manual.tooltip().as_deref(),
            Some("Download Maze 0.3.0. Move Maze to Applications to get automatic updates.")
        );
    }

    #[test]
    fn errors_are_unobtrusive() {
        // Disabled builds never show anything here.
        assert_eq!(
            title_bar_state(
                &errored(UpdateError::Disabled(
                    "Maze Dev builds don't update themselves".into()
                )),
                StateContext {
                    manual_check: true,
                    ..StateContext::default()
                }
            ),
            TitleBarState::Hidden
        );
        // Network errors only when the user asked.
        let offline = errored(UpdateError::Http("connection refused".into()));
        assert_eq!(
            title_bar_state(&offline, background()),
            TitleBarState::Hidden
        );
        assert_eq!(
            title_bar_state(
                &offline,
                StateContext {
                    manual_check: true,
                    ..StateContext::default()
                }
            ),
            TitleBarState::Error {
                message: "Update failed: network error: connection refused".into()
            }
        );
        // Everything else (a bad signature, an install that failed) is worth a look.
        let bad = title_bar_state(
            &errored(UpdateError::Manifest(ManifestError::MissingSignature)),
            background(),
        );
        assert_eq!(bad.kind(), "error");
        assert_eq!(bad.label(), None);
        assert_eq!(
            bad.tooltip().as_deref(),
            Some("Update failed: the update manifest is not signed (click to dismiss)")
        );
        let install = title_bar_state(
            &errored(UpdateError::Install(InstallError::MissingBundle(
                "Maze.app".into(),
            ))),
            background(),
        );
        assert_eq!(
            install,
            TitleBarState::Error {
                message: "Update failed: the update does not contain Maze.app".into()
            }
        );
    }
}

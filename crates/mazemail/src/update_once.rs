//! `mazemail --check-update-once` (debug builds only): runs the updater once without a
//! window and exits. The end-to-end update test (`cargo xtask update-e2e`) uses it to
//! update a real bundle from a local release server.
//!
//! It does what the app's updater does (same configuration from the running build,
//! `settings.json` and the data directory, same bundle installer), except that
//! `--update-base-url` replaces `https://releases.mazemail.dev` and
//! `--skip-codesign` accepts unsigned bundles.

#![allow(
    clippy::print_stdout,
    reason = "a command-line entry point: the outcome goes to stdout"
)]

use std::sync::Arc;

use anyhow::{Context as _, anyhow, bail};
use auto_update::{BundleInstaller, CheckOutcome, CodesignCheck, Updater, UpdaterConfig};
use clap::Args;
use release_channel::{AppVersion, ReleaseChannel};
use settings::Paths;

/// Hidden options of debug builds.
#[derive(Debug, Clone, Args)]
pub(crate) struct UpdateOnce {
    /// (debug builds) Check for an update, install it, print the outcome and exit.
    #[arg(long, hide = true)]
    pub(crate) check_update_once: bool,
    /// (debug builds) The manifest host instead of https://releases.mazemail.dev.
    #[arg(long, hide = true, value_name = "URL", requires = "check_update_once")]
    update_base_url: Option<String>,
    /// (debug builds) Accept bundles without a valid code signature.
    #[arg(long, hide = true, requires = "check_update_once")]
    skip_codesign: bool,
}

/// Checks once and installs a newer version into the running bundle.
pub(crate) fn run(options: &UpdateOnce) -> anyhow::Result<()> {
    let paths = Paths::current().context("finding the data directory")?;
    let loaded = settings::load_settings(&paths.settings_file());
    let app = AppVersion::current();
    let channel = loaded
        .settings
        .effective_update_channel(ReleaseChannel::current());
    let mut config = UpdaterConfig::for_current_build(paths.data_dir(), channel);
    if let Some(url) = &options.update_base_url {
        config.manifest_base_url = url.clone();
    }
    let mut installer = BundleInstaller::for_running_app(paths.data_dir())?;
    if options.skip_codesign {
        installer = installer.with_codesign(CodesignCheck::Skip);
    }
    installer.check_writable()?;
    let bundle = installer.installed_bundle().to_path_buf();
    println!(
        "mazemail {} ({}) in {}, channel {}, data {}",
        app.semver,
        app.channel.name(),
        bundle.display(),
        channel.name(),
        paths.data_dir().display()
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?;
    runtime.block_on(async {
        let updater = Updater::new(config, Arc::new(installer))?;
        if let Some(reason) = updater.disabled_reason() {
            bail!("updates are off: {reason}");
        }
        let outcome = updater
            .check_and_download()
            .await
            .map_err(|error| anyhow!("update failed: {error}"))?;
        match outcome {
            CheckOutcome::UpToDate => println!("up to date"),
            CheckOutcome::Available(version) => println!("available: {version} (not installed)"),
            CheckOutcome::Installed(version) => {
                println!("installed {version} into {}", bundle.display());
            }
            CheckOutcome::Busy => println!("busy"),
        }
        Ok(())
    })
}

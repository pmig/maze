//! `mazemail`: starts Maze, serves it headless, or runs a CLI subcommand (PLAN §4.11).
//!
//! - `mazemail` opens the app (`MAZEMAIL_DEMO=1`: with synthetic mail, no core).
//! - `mazemail --headless` opens the core without a window and serves the local API
//!   socket until SIGINT or SIGTERM.
//! - Debug builds only: `mazemail --check-update-once [--update-base-url <url>]
//!   [--skip-codesign]` runs the updater once without a window (the end-to-end update
//!   test, `cargo xtask update-e2e`).
//! - `mazemail <command>` is a thin client of the running Maze: it sends one request
//!   over the socket with the token from `MAZEMAIL_SESSION` and prints the result as
//!   text or, with `--json`, as JSON. Exit codes: 0 ok, 1 failure, 2 usage,
//!   3 permission denied, 4 approval denied, 5 not running, 6 not signed in.

mod app;
mod cli;
mod headless;
#[cfg(debug_assertions)]
mod update_once;

use std::process::ExitCode;
use std::time::Instant;

use clap::{Args, FromArgMatches as _};

/// Options before (or instead of) a subcommand.
#[derive(Debug, Clone, Copy, Args)]
struct Global {
    /// Run without a window: open the mail data and serve the local API socket until
    /// interrupted (scripts, servers, tests).
    #[arg(long)]
    headless: bool,
    /// Print the command's result as JSON (see `mazemail help agents` for the shapes).
    #[arg(long, global = true)]
    json: bool,
}

/// The clap command: the global options plus every registry subcommand.
fn command() -> clap::Command {
    let base = clap::Command::new(mazemail_types::TECHNICAL_NAME)
        .version(release_channel::VERSION)
        .args_conflicts_with_subcommands(true);
    let base = Global::augment_args(base);
    #[cfg(debug_assertions)]
    let base = update_once::UpdateOnce::augment_args(base);
    // Set after `augment`: the registry's subcommand derive brings its own `about`.
    local_api::registry::cli::augment(base).about(format!(
        "{}, a local-first email client. Without a command, starts the app.",
        mazemail_types::PRODUCT_NAME
    ))
}

fn init_tracing(default: &str) {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MAZEMAIL_LOG")
                .unwrap_or_else(|_| default.into()),
        )
        .with_writer(std::io::stderr)
        .init();
}

fn main() -> ExitCode {
    // Taken first: the startup measurement runs from here to the first frame.
    let process_start = Instant::now();
    // `time` only reads the local UTC offset reliably while the process has a single
    // thread, so read it before anything spawns one.
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

    let matches = command().get_matches();
    let global = match Global::from_arg_matches(&matches) {
        Ok(global) => global,
        Err(error) => error.exit(),
    };

    if matches.subcommand().is_some() {
        // Quiet by default: stdout is the command's output, stderr its errors.
        init_tracing("warn");
        return match local_api::registry::cli::CliCommand::from_arg_matches(&matches) {
            Ok(command) => cli::run(command, global.json),
            Err(error) => error.exit(),
        };
    }

    init_tracing("info");
    #[cfg(debug_assertions)]
    {
        let update_once = match update_once::UpdateOnce::from_arg_matches(&matches) {
            Ok(options) => options,
            Err(error) => error.exit(),
        };
        if update_once.check_update_once {
            return match update_once::run(&update_once) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!("{error:#}");
                    ExitCode::FAILURE
                }
            };
        }
    }
    let result = if global.headless {
        headless::run(local_offset)
    } else {
        app::run(app::AppOptions {
            process_start,
            local_offset,
        })
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_tree_is_valid() {
        command().debug_assert();
    }

    #[test]
    fn help_describes_the_app() {
        let about = command().get_about().map(ToString::to_string);
        assert_eq!(
            about.as_deref(),
            Some("Maze, a local-first email client. Without a command, starts the app.")
        );
    }

    #[test]
    fn global_options_and_subcommands() {
        let matches = command()
            .try_get_matches_from(["mazemail", "search", "from:anna", "--json"])
            .unwrap();
        assert!(Global::from_arg_matches(&matches).unwrap().json);
        assert!(matches.subcommand().is_some());

        let matches = command()
            .try_get_matches_from(["mazemail", "--headless"])
            .unwrap();
        let global = Global::from_arg_matches(&matches).unwrap();
        assert!(global.headless && matches.subcommand().is_none());

        #[cfg(debug_assertions)]
        {
            let matches = command()
                .try_get_matches_from([
                    "mazemail",
                    "--check-update-once",
                    "--update-base-url",
                    "http://127.0.0.1:8000",
                    "--skip-codesign",
                ])
                .unwrap();
            let options = update_once::UpdateOnce::from_arg_matches(&matches).unwrap();
            assert!(options.check_update_once);
            // The update options need `--check-update-once`.
            assert!(
                command()
                    .try_get_matches_from(["mazemail", "--skip-codesign"])
                    .is_err()
            );
        }

        // `--headless` is no option of a command.
        assert!(
            command()
                .try_get_matches_from(["mazemail", "--headless", "status"])
                .is_err()
        );
    }
}

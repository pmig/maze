//! `mazemail`: starts Maze, serves it headless, or runs a CLI subcommand (PLAN §4.11).
//!
//! - `mazemail` opens the app (`MAZEMAIL_DEMO=1`: with synthetic mail, no core).
//! - `mazemail <mailto:…>` (what the system runs for a `mailto:` link) opens a new
//!   message for the link in the running Maze, or starts Maze with it.
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
#[derive(Debug, Clone, Args)]
struct Global {
    /// Run without a window: open the mail data and serve the local API socket until
    /// interrupted (scripts, servers, tests).
    #[arg(long)]
    headless: bool,
    /// Print the command's result as JSON (see `mazemail help agents` for the shapes).
    #[arg(long, global = true)]
    json: bool,
    /// A `mailto:` link to write to (the system passes it when Maze handles mail links).
    #[arg(value_name = "URL", value_parser = mailto_link)]
    url: Option<String>,
}

/// Accepts only `mailto:` links as the app's argument.
fn mailto_link(value: &str) -> Result<String, String> {
    let is_mailto = value
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mailto:"));
    if is_mailto {
        Ok(value.to_owned())
    } else {
        Err("only mailto: links (or a command, see `mazemail help`)".to_owned())
    }
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
    if let Some(url) = &global.url
        && !global.headless
        && let Some(code) = hand_to_running(url)
    {
        return code;
    }
    let result = if global.headless {
        headless::run(local_offset)
    } else {
        app::run(app::AppOptions {
            process_start,
            local_offset,
            open_url: global.url,
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

/// Hands `url` to the Maze that is already running (`mazemail open`). `None` if none
/// is: this process starts Maze with the link.
fn hand_to_running(url: &str) -> Option<ExitCode> {
    let paths = settings::Paths::current().ok()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let answer = runtime.block_on(async {
        let endpoint = local_api::Endpoint::discover(&paths);
        let mut client = local_api::LocalApiClient::connect(&endpoint).await?;
        client.call("open", serde_json::json!({ "url": url })).await
    });
    match answer {
        Ok(_) => Some(ExitCode::SUCCESS),
        Err(local_api::ClientError::NotRunning { .. }) => None,
        Err(error) => {
            tracing::error!(%error, "the running Maze didn't open the link");
            Some(ExitCode::FAILURE)
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

        // A mailto: link (what the system runs for one), but nothing else, as the URL.
        let matches = command()
            .try_get_matches_from(["mazemail", "mailto:anna@example.com?subject=Hi"])
            .unwrap();
        let global = Global::from_arg_matches(&matches).unwrap();
        assert_eq!(
            global.url.as_deref(),
            Some("mailto:anna@example.com?subject=Hi")
        );
        assert!(matches.subcommand().is_none());
        assert!(
            command()
                .try_get_matches_from(["mazemail", "https://example.com"])
                .is_err()
        );
        let matches = command()
            .try_get_matches_from(["mazemail", "status"])
            .unwrap();
        assert!(matches.subcommand().is_some(), "commands still win");

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

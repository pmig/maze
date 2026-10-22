//! `mazemail`: starts Maze, or runs a CLI subcommand.

mod app;

use std::time::Instant;

use clap::{Parser, Subcommand};
use mazemail_types::PRODUCT_NAME;

#[derive(Debug, Parser)]
#[command(
    name = "mazemail",
    version,
    about = "Maze, a local-first email client. Without a subcommand, starts the app."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Show whether Maze is running and which accounts it knows.
    Status,
}

fn main() -> anyhow::Result<()> {
    // Taken first: the startup measurement runs from here to the first frame.
    let process_start = Instant::now();
    // `time` only reads the local UTC offset reliably while the process has a single
    // thread, so read it before anything spawns one.
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MAZEMAIL_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        None => app::run(app::AppOptions {
            process_start,
            local_offset,
        }),
        Some(Command::Status) => {
            tracing::info!("{PRODUCT_NAME} is not running");
            Ok(())
        }
    }
}

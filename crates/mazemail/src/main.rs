//! `mazemail`: starts Maze, or runs a CLI subcommand.

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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MAZEMAIL_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        None => tracing::info!("{PRODUCT_NAME} has no window yet"),
        Some(Command::Status) => tracing::info!("{PRODUCT_NAME} is not running"),
    }
    Ok(())
}

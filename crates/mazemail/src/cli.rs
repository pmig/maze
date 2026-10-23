//! `mazemail <command>`: a thin client of the running Maze (PLAN §4.11).
//!
//! The command line is parsed by the registry's clap tree, turned into a request
//! ([`CliCommand::into_invocation`] reads files named on the command line and resolves
//! relative paths, because Maze must never open paths on a caller's behalf), sent over
//! the socket with the token from `MAZEMAIL_SESSION`, and the result is printed with
//! [`render_human`] or as JSON. Progress ("waiting for approval in Maze…") goes to
//! stderr. `mazemail help agents` and `mazemail help <command>` work offline.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "a command-line client: results go to stdout, errors and progress to stderr"
)]

use std::process::ExitCode;

use local_api::registry::cli::{CliCommand, SystemEnv, local_help};
use local_api::registry::render::render_human;
use local_api::{ClientError, ExitCode as ApiExit, LocalApiClient, Progress, ProgressCode};
use serde_json::Value;
use settings::Paths;

/// A failed command: what to print and the exit code.
#[derive(Debug)]
struct Failure {
    code: ApiExit,
    message: String,
}

impl Failure {
    fn new(code: ApiExit, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<ClientError> for Failure {
    fn from(error: ClientError) -> Self {
        Self::new(error.exit_code(), error.to_string())
    }
}

impl From<local_api::registry::cli::CliError> for Failure {
    fn from(error: local_api::registry::cli::CliError) -> Self {
        Self::new(error.exit_code(), error.to_string())
    }
}

/// Runs one command and returns the process exit code.
pub(crate) fn run(command: CliCommand, json: bool) -> ExitCode {
    match execute(command, json) {
        Ok(output) => {
            print!("{output}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("mazemail: {}", failure.message);
            ExitCode::from(failure.code.code())
        }
    }
}

/// Runs a command and returns what to print on stdout.
fn execute(command: CliCommand, json: bool) -> Result<String, Failure> {
    if let CliCommand::Help(args) = &command
        && !args.topic.is_empty()
    {
        return local_help(&args.topic).ok_or_else(|| {
            Failure::new(
                ApiExit::Usage,
                format!(
                    "no help for `{}` (see `mazemail help`)",
                    args.topic.join(" ")
                ),
            )
        });
    }
    let is_help = matches!(command, CliCommand::Help(_));
    let invocation = command.into_invocation(&SystemEnv)?;
    let paths =
        Paths::current().map_err(|error| Failure::new(ApiExit::Failure, error.to_string()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| Failure::new(ApiExit::Failure, format!("cannot start: {error}")))?;
    let data = runtime.block_on(async {
        let mut client = LocalApiClient::connect_default(&paths).await?;
        client
            .call_with_progress(invocation.cmd(), invocation.args.clone(), report_progress)
            .await
    });
    let data = match data {
        Ok(data) => data,
        // The command list is useful without a running Maze too.
        Err(ClientError::NotRunning { .. }) if is_help => {
            return Ok(crate::command().render_long_help().to_string());
        }
        Err(error) => return Err(error.into()),
    };
    let data = invocation.finish(data, &SystemEnv)?;
    Ok(format_output(
        invocation.spec,
        &data,
        &invocation.render,
        json,
    ))
}

fn report_progress(progress: &Progress) {
    match (progress.code, progress.done, progress.total) {
        (ProgressCode::Working, Some(done), Some(total)) => {
            eprintln!("{} ({done}/{total})", progress.message);
        }
        (ProgressCode::Working, Some(done), None) => eprintln!("{} ({done})", progress.message),
        _ => eprintln!("{}", progress.message),
    }
}

/// The output as people or programs read it.
fn format_output(
    spec: &local_api::registry::CommandSpec,
    data: &Value,
    render: &local_api::registry::cli::RenderOptions,
    json: bool,
) -> String {
    if json {
        let mut text = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
        text.push('\n');
        text
    } else {
        render_human(spec, data, render)
    }
}

#[cfg(test)]
mod tests {
    use clap::FromArgMatches as _;
    use serde_json::json;

    use super::*;

    fn invocation(args: &[&str]) -> local_api::registry::cli::Invocation {
        let matches = crate::command()
            .try_get_matches_from(std::iter::once("mazemail").chain(args.iter().copied()))
            .unwrap();
        CliCommand::from_arg_matches(&matches)
            .unwrap()
            .into_invocation(&SystemEnv)
            .unwrap()
    }

    #[test]
    fn arguments_become_requests() {
        let search = invocation(&["search", "from:anna has:attachment", "--limit", "5"]);
        assert_eq!(search.cmd(), "search");
        assert_eq!(
            search.args,
            json!({ "query": "from:anna has:attachment", "limit": 5 })
        );

        let show = invocation(&["show", "t_19bd", "--format", "md"]);
        assert_eq!(show.args, json!({ "id": "t_19bd", "format": "md" }));

        let login = invocation(&["login", "--profile", "read-only", "--ttl", "1h"]);
        assert_eq!(login.cmd(), "login");
        assert_eq!(login.args["profile"], "read-only");
        assert_eq!(login.args["ttl"], "1h");

        let archive = invocation(&["archive", "m_b", "t_c"]);
        assert_eq!(archive.args, json!({ "ids": ["m_b", "t_c"] }));

        // Paths are resolved here, against our working directory, never by Maze.
        let import = invocation(&["import", "old.mbox", "--format", "mbox"]);
        let path = import.args["path"].as_str().unwrap();
        assert!(std::path::Path::new(path).is_absolute(), "{path}");
        assert!(path.ends_with("old.mbox"));
        assert_eq!(import.args["format"], "mbox");
    }

    #[test]
    fn output_is_json_or_text() {
        let spec = local_api::registry::find("show").unwrap();
        let data = json!({ "id": "m_b", "thread": "t_c", "format": "md", "content": "## Hi\n", "messages": [] });
        let render = local_api::registry::cli::RenderOptions::default();
        assert_eq!(format_output(spec, &data, &render, false), "## Hi\n");
        let printed = format_output(spec, &data, &render, true);
        assert_eq!(serde_json::from_str::<Value>(&printed).unwrap(), data);
    }

    #[test]
    fn help_topics_work_offline() {
        let agents = execute(
            CliCommand::Help(local_api::registry::args::HelpArgs {
                topic: vec!["agents".into()],
            }),
            false,
        )
        .unwrap();
        assert_eq!(agents, local_api::registry::agents_md());
        let unknown = execute(
            CliCommand::Help(local_api::registry::args::HelpArgs {
                topic: vec!["nope".into()],
            }),
            false,
        )
        .unwrap_err();
        assert_eq!(unknown.code, ApiExit::Usage);
    }
}

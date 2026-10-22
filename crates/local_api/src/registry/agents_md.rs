//! AGENTS.md: what `mazemail` can do, generated from the registry (CONCEPT §10.2).

use std::fmt::Write as _;

use clap::ArgAction;

use super::cli::cli_command;
use super::shape::shape;
use super::{CommandSpec, registry};
use crate::permission::Access;
use crate::protocol::ExitCode;
use crate::token::Profile;

/// The AGENTS.md text that Maze puts into agent session folders and that
/// `mazemail help agents` prints.
pub fn agents_md() -> String {
    let mut out = String::new();
    // `write!` into a `String` can't fail.
    let _ = write_agents_md(&mut out);
    out
}

fn write_agents_md(out: &mut String) -> std::fmt::Result {
    let cli = cli_command();
    writeln!(out, "# Mail access with the `mazemail` CLI")?;
    writeln!(out)?;
    writeln!(
        out,
        "<!-- Generated from the Maze command registry (`local_api`). Do not edit. -->"
    )?;
    writeln!(out)?;
    out.push_str(INTRO);
    writeln!(out)?;

    writeln!(out, "## Exit codes")?;
    writeln!(out)?;
    writeln!(out, "| Code | Meaning |")?;
    writeln!(out, "| ---- | ------- |")?;
    for code in ExitCode::ALL {
        writeln!(out, "| {} | {} |", code.code(), code.description())?;
    }
    writeln!(out)?;

    writeln!(out, "## What each profile may do")?;
    writeln!(out)?;
    writeln!(
        out,
        "`allow` runs directly, `propose` becomes a proposal you review in Maze, `ask` waits \
         for approval in Maze, `deny` fails with exit code 3."
    )?;
    writeln!(out)?;
    write!(out, "| Command |")?;
    for profile in Profile::ALL {
        write!(out, " {profile} |")?;
    }
    writeln!(out)?;
    writeln!(out, "| ------- |{}", " --- |".repeat(Profile::ALL.len()))?;
    for spec in registry() {
        write!(out, "| `{}` |", spec.cli_name())?;
        for profile in Profile::ALL {
            write!(out, " {} |", access_cell(spec, profile))?;
        }
        writeln!(out)?;
    }
    writeln!(out)?;
    writeln!(
        out,
        "Permanent delete is not available to tokens. `send` is disabled unless the token was \
         approved with `--allow-send`, and then every send asks."
    )?;
    writeln!(out)?;

    writeln!(out, "## Commands")?;
    for spec in registry() {
        writeln!(out)?;
        write_command(out, &cli, spec)?;
    }
    Ok(())
}

const INTRO: &str = "\
`mazemail` talks to the running Maze app over a local socket; it never opens the mail \
store itself. Every command prints a readable table on a terminal and stable JSON with \
`--json`. Ids are short and stable: `m_…` message, `t_…` thread, `d_…` draft, `f_…` \
folder, `a_…` account, `tk_…` token.

## Signing in

- Commands that touch mail need a session token in `MAZEMAIL_SESSION`. Agents started by \
  Maze already have one.
- Otherwise run `eval \"$(mazemail login --profile read-only --ttl 8h)\"`. Maze shows which \
  program is asking and waits for approval (\"waiting for approval in Maze…\").
- Without a token only `mazemail help`, `mazemail status` and `mazemail login` work (exit \
  code 6).
- Your token decides what you can do (see the table below) and which accounts and folders \
  you can see. Accounts whose AI policy excludes agents are never visible.
";

fn access_cell(spec: &CommandSpec, profile: Profile) -> String {
    if !spec.requires_token {
        return match spec.access(profile, false) {
            Access::Ask => "ask (no token needed)".to_owned(),
            other => format!("{other} (no token needed)"),
        };
    }
    let plain = spec.access(profile, false);
    let with_send = spec.access(profile, true);
    if plain == with_send {
        plain.to_string()
    } else {
        format!("{plain} ({with_send} with `--allow-send`)")
    }
}

fn write_command(out: &mut String, cli: &clap::Command, spec: &CommandSpec) -> std::fmt::Result {
    writeln!(out, "### `mazemail {}`", spec.cli_name())?;
    writeln!(out)?;
    writeln!(out, "{}.", spec.summary)?;
    writeln!(out)?;
    writeln!(out, "{}", spec.help)?;
    writeln!(out)?;
    writeln!(
        out,
        "Permission: `{}`{}",
        spec.permission,
        if spec.requires_token {
            ""
        } else {
            " (works without a token)"
        }
    )?;
    writeln!(out)?;

    let Some(sub) = spec
        .path
        .iter()
        .try_fold(cli, |cmd, word| cmd.find_subcommand(word))
    else {
        return Ok(());
    };
    let args: Vec<&clap::Arg> = sub
        .get_arguments()
        .filter(|arg| !matches!(arg.get_id().as_str(), "help" | "version"))
        .collect();
    writeln!(out, "```sh\n{}\n```", usage(spec, &args))?;
    writeln!(out)?;
    if !args.is_empty() {
        writeln!(out, "| Argument | Description |")?;
        writeln!(out, "| -------- | ----------- |")?;
        for arg in &args {
            writeln!(out, "| `{}` | {} |", arg_name(arg), arg_description(arg))?;
        }
        writeln!(out)?;
    }

    writeln!(out, "Output (`--json`):")?;
    writeln!(out)?;
    writeln!(
        out,
        "```ts\n{}\n```",
        shape(&(spec.output_schema)().to_value())
    )?;
    writeln!(out)?;
    writeln!(out, "Examples:")?;
    writeln!(out)?;
    writeln!(out, "```sh")?;
    for example in spec.examples {
        writeln!(out, "# {}", example.description)?;
        writeln!(out, "{}", example.command)?;
    }
    writeln!(out, "```")?;
    Ok(())
}

fn is_flag(arg: &clap::Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::SetTrue | ArgAction::SetFalse | ArgAction::Count
    )
}

fn is_multiple(arg: &clap::Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Append)
}

fn value_name(arg: &clap::Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map_or_else(|| arg.get_id().as_str().to_uppercase(), ToString::to_string)
}

fn arg_name(arg: &clap::Arg) -> String {
    let dots = if is_multiple(arg) { "..." } else { "" };
    if arg.is_positional() {
        return format!("<{}>{dots}", value_name(arg));
    }
    let long = arg.get_long().unwrap_or_else(|| arg.get_id().as_str());
    if is_flag(arg) {
        format!("--{long}")
    } else {
        format!("--{long} <{}>{dots}", value_name(arg))
    }
}

fn arg_description(arg: &clap::Arg) -> String {
    let mut text = arg
        .get_help()
        .map(|help| help.to_string())
        .unwrap_or_default()
        .replace('|', "\\|")
        .replace('\n', " ");
    if !text.is_empty() && !text.ends_with(['.', '!', '?']) {
        text.push('.');
    }
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| format!("`{}`", value.get_name()))
        .collect();
    if !possible.is_empty() && !is_flag(arg) {
        text.push_str(&format!(" One of {}.", possible.join(", ")));
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if !defaults.is_empty() && !is_flag(arg) {
        text.push_str(&format!(" Default: `{}`.", defaults.join(",")));
    }
    if arg.is_positional() {
        // The usage line shows whether a positional argument is required.
    } else if arg.is_required_set() {
        text.push_str(" Required.");
    } else if is_multiple(arg) && !text.to_lowercase().contains("repeatable") {
        text.push_str(" Repeatable.");
    }
    text.trim().to_owned()
}

fn usage(spec: &CommandSpec, args: &[&clap::Arg]) -> String {
    let mut parts = vec![format!("mazemail {}", spec.cli_name())];
    let has_optional = args
        .iter()
        .any(|arg| !arg.is_positional() && !arg.is_required_set());
    if has_optional {
        parts.push("[OPTIONS]".to_owned());
    }
    for arg in args
        .iter()
        .filter(|arg| !arg.is_positional() && arg.is_required_set())
    {
        parts.push(arg_name(arg));
    }
    for arg in args.iter().filter(|arg| arg.is_positional()) {
        let name = arg_name(arg);
        parts.push(if arg.is_required_set() {
            name
        } else {
            format!("[{name}]")
        });
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::SESSION_ENV;

    #[test]
    fn agents_md_snapshot() {
        insta::assert_snapshot!(agents_md());
    }

    #[test]
    fn mentions_every_command_and_no_secrets() {
        let text = agents_md();
        for spec in registry() {
            assert!(text.contains(&format!("### `mazemail {}`", spec.cli_name())));
        }
        assert!(text.contains(SESSION_ENV));
        assert!(!text.contains("mazemail_s_"));
    }
}

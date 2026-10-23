//! The CLI side of the registry: the clap command tree and the conversion from parsed
//! arguments to a request.
//!
//! The `mazemail` binary embeds [`CliCommand`] as its subcommand enum, builds the clap
//! command with [`augment`] (which applies summaries, help and examples from the
//! registry), turns the parsed command into an [`Invocation`] with
//! [`CliCommand::into_invocation`], sends it with
//! [`LocalApiClient`](crate::LocalApiClient), passes the result through
//! [`Invocation::finish`] and prints it with `--json` or
//! [`render_human`](super::render::render_human).

use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use clap::Subcommand;
use mazemail_types::Sensitive;
use serde::Serialize;
use serde_json::Value;

use super::args::*;
use super::output::{AttachmentData, AttachmentSaved};
use super::{CommandSpec, GROUPS, find, find_by_path, registry};
use crate::protocol::ExitCode;

/// Every `mazemail` subcommand.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum CliCommand {
    /// `mazemail status`.
    Status(StatusArgs),
    /// `mazemail help`.
    Help(HelpArgs),
    /// `mazemail login`.
    Login(LoginArgs),
    /// `mazemail token …`.
    #[command(subcommand)]
    Token(TokenCommand),
    /// `mazemail search`.
    Search(SearchArgs),
    /// `mazemail show`.
    Show(ShowArgs),
    /// `mazemail folders`.
    Folders(FoldersArgs),
    /// `mazemail tags`.
    Tags(TagsArgs),
    /// `mazemail contacts`.
    Contacts(ContactsArgs),
    /// `mazemail identities`.
    Identities(IdentitiesArgs),
    /// `mazemail attachment …`.
    #[command(subcommand)]
    Attachment(AttachmentCommand),
    /// `mazemail draft …`.
    #[command(subcommand)]
    Draft(DraftCommand),
    /// `mazemail archive`.
    Archive(ArchiveArgs),
    /// `mazemail move`.
    Move(MoveArgs),
    /// `mazemail tag`.
    Tag(TagArgs),
    /// `mazemail flag`.
    Flag(FlagArgs),
    /// `mazemail read`.
    Read(ReadArgs),
    /// `mazemail propose`.
    Propose(ProposeCliArgs),
    /// `mazemail rule …`.
    #[command(subcommand)]
    Rule(RuleCommand),
    /// `mazemail send`.
    Send(SendArgs),
    /// `mazemail export`.
    Export(ExportArgs),
    /// `mazemail import`.
    Import(ImportArgs),
    /// `mazemail account …`.
    #[command(subcommand)]
    Account(AccountCommand),
    /// `mazemail sync`.
    Sync(SyncArgs),
}

/// `mazemail account …`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum AccountCommand {
    /// `mazemail account list`.
    List(AccountListArgs),
    /// `mazemail account add`.
    Add(AccountAddCliArgs),
    /// `mazemail account remove`.
    Remove(AccountRemoveArgs),
}

/// `mazemail token …`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum TokenCommand {
    /// `mazemail token create`.
    Create(LoginArgs),
    /// `mazemail token list`.
    List(TokenListArgs),
    /// `mazemail token revoke`.
    Revoke(TokenRevokeArgs),
}

/// `mazemail attachment …`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum AttachmentCommand {
    /// `mazemail attachment save`.
    Save(AttachmentSaveCliArgs),
}

/// `mazemail draft …`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum DraftCommand {
    /// `mazemail draft new`.
    New(DraftNewCliArgs),
    /// `mazemail draft edit`.
    Edit(DraftEditCliArgs),
}

/// `mazemail rule …`.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RuleCommand {
    /// `mazemail rule add`.
    Add(RuleAddArgs),
}

/// Adds every subcommand to `cmd`, with summaries, help and examples from the registry.
/// clap's own `help` subcommand is replaced by `mazemail help`.
pub fn augment(cmd: clap::Command) -> clap::Command {
    use clap::Subcommand as _;
    let mut cmd = CliCommand::augment_subcommands(cmd.disable_help_subcommand(true));
    for (group, about) in GROUPS {
        cmd = mut_path(cmd, &[group], &|sub| {
            sub.about(*about)
                .subcommand_required(true)
                .arg_required_else_help(true)
        });
    }
    for spec in registry() {
        cmd = mut_path(cmd, spec.path, &|sub| {
            sub.about(spec.summary)
                .long_about(format!("{}\n\n{}", spec.summary, spec.help))
                .after_long_help(examples_text(spec))
        });
    }
    cmd
}

/// The full `mazemail` clap command (subcommands only).
pub fn cli_command() -> clap::Command {
    augment(clap::Command::new(mazemail_types::TECHNICAL_NAME))
}

fn mut_path(
    cmd: clap::Command,
    path: &[&str],
    f: &dyn Fn(clap::Command) -> clap::Command,
) -> clap::Command {
    let Some((first, rest)) = path.split_first() else {
        return f(cmd);
    };
    if cmd.find_subcommand(first).is_none() {
        // The registry test keeps the tree and the registry in sync.
        return cmd;
    }
    cmd.mut_subcommand(*first, |sub| mut_path(sub, rest, f))
}

fn examples_text(spec: &CommandSpec) -> String {
    let mut text = String::from("Examples:\n");
    for example in spec.examples {
        text.push_str(&format!(
            "  # {}\n  {}\n",
            example.description, example.command
        ));
    }
    text
}

/// Text for `mazemail help <topic>` that the CLI can print without asking Maze:
/// AGENTS.md for `agents`, a command's long help for a command path.
pub fn local_help(topic: &[String]) -> Option<String> {
    if topic.len() == 1 && topic[0] == "agents" {
        return Some(super::agents_md());
    }
    let path: Vec<&str> = topic.iter().map(String::as_str).collect();
    let mut cmd = cli_command();
    let mut current = &mut cmd;
    for word in &path {
        current = current.find_subcommand_mut(word)?;
    }
    let mut help = current
        .clone()
        .bin_name(format!("mazemail {}", path.join(" ")));
    Some(help.render_long_help().to_string())
}

/// Where the CLI reads and writes local files. Abstracted for tests.
pub trait ClientEnv {
    /// The working directory (relative paths are resolved against it).
    fn cwd(&self) -> io::Result<PathBuf>;
    /// Reads a file.
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Reads all of stdin.
    fn read_stdin(&self) -> io::Result<Vec<u8>>;
    /// Whether something exists at `path`.
    fn exists(&self, path: &Path) -> bool;
    /// Creates `path` (failing if it exists) and writes `bytes`.
    fn write_new_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Asks for a password on the terminal without echoing it.
    fn read_password(&self, prompt: &str) -> io::Result<String>;
}

/// The real process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemEnv;

impl ClientEnv for SystemEnv {
    fn cwd(&self) -> io::Result<PathBuf> {
        std::env::current_dir()
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn read_stdin(&self) -> io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        io::stdin().lock().read_to_end(&mut buffer)?;
        Ok(buffer)
    }

    fn exists(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok()
    }

    fn write_new_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    fn read_password(&self, prompt: &str) -> io::Result<String> {
        // Reads from the controlling terminal (`/dev/tty`), not from stdin.
        rpassword::prompt_password(prompt)
    }
}

/// Errors preparing a request or finishing its result on the CLI side.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A file named on the command line can't be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// A result file can't be written.
    #[error("cannot write {path}: {source}")]
    Write {
        /// The file.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// Input that doesn't make sense (a file that isn't UTF-8 or valid JSON, …).
    #[error("{0}")]
    Invalid(String),
    /// Maze returned something this CLI can't process.
    #[error("unexpected response from Maze: {0}")]
    Response(String),
}

impl CliError {
    /// The process exit code.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Read { .. } | Self::Invalid(_) => ExitCode::Usage,
            Self::Write { .. } | Self::Response(_) => ExitCode::Failure,
        }
    }
}

/// How the CLI prints the result without `--json`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderOptions {
    /// Print only the `id` of each row (`search --ids`).
    pub ids_only: bool,
}

/// A request ready to send, plus what the CLI does with the result.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// The command.
    pub spec: &'static CommandSpec,
    /// The wire arguments.
    pub args: Value,
    /// Printing options.
    pub render: RenderOptions,
    post: PostAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PostAction {
    None,
    SaveAttachment { dir: PathBuf },
}

impl Invocation {
    fn new(name: &str, args: &impl Serialize) -> Result<Self, CliError> {
        let spec =
            find(name).ok_or_else(|| CliError::Invalid(format!("unknown command {name}")))?;
        let args =
            serde_json::to_value(args).map_err(|error| CliError::Invalid(error.to_string()))?;
        Ok(Self {
            spec,
            args,
            render: RenderOptions::default(),
            post: PostAction::None,
        })
    }

    /// The command's wire name.
    pub fn cmd(&self) -> &'static str {
        self.spec.name
    }

    /// Runs the CLI-side part of the command on Maze's response (for `attachment save`:
    /// writes the file) and returns what to print.
    pub fn finish(&self, data: Value, env: &dyn ClientEnv) -> Result<Value, CliError> {
        match &self.post {
            PostAction::None => Ok(data),
            PostAction::SaveAttachment { dir } => {
                let attachment: AttachmentData = serde_json::from_value(data)
                    .map_err(|error| CliError::Response(error.to_string()))?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(attachment.data.expose())
                    .map_err(|error| CliError::Response(format!("attachment data: {error}")))?;
                let path = unique_path(env, dir, &sanitize_file_name(attachment.filename.expose()));
                env.write_new_file(&path, &bytes)
                    .map_err(|source| CliError::Write {
                        path: path.clone(),
                        source,
                    })?;
                let saved = AttachmentSaved {
                    id: attachment.id,
                    path,
                    content_type: attachment.content_type,
                    size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                };
                serde_json::to_value(saved).map_err(|error| CliError::Response(error.to_string()))
            }
        }
    }
}

impl CliCommand {
    /// The registry entry of this command.
    pub fn spec(&self) -> &'static CommandSpec {
        let path: &[&str] = match self {
            Self::Status(_) => &["status"],
            Self::Help(_) => &["help"],
            Self::Login(_) => &["login"],
            Self::Token(TokenCommand::Create(_)) => &["token", "create"],
            Self::Token(TokenCommand::List(_)) => &["token", "list"],
            Self::Token(TokenCommand::Revoke(_)) => &["token", "revoke"],
            Self::Search(_) => &["search"],
            Self::Show(_) => &["show"],
            Self::Folders(_) => &["folders"],
            Self::Tags(_) => &["tags"],
            Self::Contacts(_) => &["contacts"],
            Self::Identities(_) => &["identities"],
            Self::Attachment(AttachmentCommand::Save(_)) => &["attachment", "save"],
            Self::Draft(DraftCommand::New(_)) => &["draft", "new"],
            Self::Draft(DraftCommand::Edit(_)) => &["draft", "edit"],
            Self::Archive(_) => &["archive"],
            Self::Move(_) => &["move"],
            Self::Tag(_) => &["tag"],
            Self::Flag(_) => &["flag"],
            Self::Read(_) => &["read"],
            Self::Propose(_) => &["propose"],
            Self::Rule(RuleCommand::Add(_)) => &["rule", "add"],
            Self::Send(_) => &["send"],
            Self::Export(_) => &["export"],
            Self::Import(_) => &["import"],
            Self::Account(AccountCommand::List(_)) => &["account", "list"],
            Self::Account(AccountCommand::Add(_)) => &["account", "add"],
            Self::Account(AccountCommand::Remove(_)) => &["account", "remove"],
            Self::Sync(_) => &["sync"],
        };
        // Every path above is in the registry (tested), so the fallback is unreachable.
        find_by_path(path).unwrap_or(&registry()[0])
    }

    /// Prepares the request: reads files named on the command line, resolves relative
    /// paths, and remembers what to do with the result.
    pub fn into_invocation(self, env: &dyn ClientEnv) -> Result<Invocation, CliError> {
        let name = self.spec().name;
        match self {
            Self::Status(args) => Invocation::new(name, &args),
            Self::Help(args) => Invocation::new(name, &args),
            Self::Login(args) | Self::Token(TokenCommand::Create(args)) => {
                Invocation::new(name, &args)
            }
            Self::Token(TokenCommand::List(args)) => Invocation::new(name, &args),
            Self::Token(TokenCommand::Revoke(args)) => Invocation::new(name, &args),
            Self::Search(args) => {
                let ids_only = args.ids;
                let mut invocation = Invocation::new(name, &args)?;
                invocation.render.ids_only = ids_only;
                Ok(invocation)
            }
            Self::Show(args) => Invocation::new(name, &args),
            Self::Folders(args) => Invocation::new(name, &args),
            Self::Tags(args) => Invocation::new(name, &args),
            Self::Contacts(args) => Invocation::new(name, &args),
            Self::Identities(args) => Invocation::new(name, &args),
            Self::Attachment(AttachmentCommand::Save(args)) => {
                let cwd = cwd(env)?;
                let dir = args.to.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
                let mut invocation = Invocation::new(name, &AttachmentSaveArgs { id: args.id })?;
                invocation.post = PostAction::SaveAttachment { dir };
                Ok(invocation)
            }
            Self::Draft(DraftCommand::New(args)) => {
                let body = body(env, args.body_file.as_deref(), args.body)?;
                let wire = DraftNewArgs {
                    reply_to: args.reply_to,
                    reply_all: args.reply_all,
                    forward: args.forward,
                    identity: args.identity,
                    to: args.to,
                    cc: args.cc,
                    bcc: args.bcc,
                    subject: args.subject,
                    body,
                    attachments: attachments(env, &args.attach)?,
                };
                Invocation::new(name, &wire)
            }
            Self::Draft(DraftCommand::Edit(args)) => {
                let body = body(env, args.body_file.as_deref(), args.body)?;
                let non_empty = |list: Vec<Sensitive<String>>| (!list.is_empty()).then_some(list);
                let wire = DraftEditArgs {
                    id: args.id,
                    identity: args.identity,
                    to: non_empty(args.to),
                    cc: non_empty(args.cc),
                    bcc: non_empty(args.bcc),
                    subject: args.subject,
                    body,
                    attachments: attachments(env, &args.attach)?,
                };
                Invocation::new(name, &wire)
            }
            Self::Archive(args) => Invocation::new(name, &args),
            Self::Move(args) => Invocation::new(name, &args),
            Self::Tag(args) => Invocation::new(name, &args),
            Self::Flag(args) => Invocation::new(name, &args),
            Self::Read(args) => Invocation::new(name, &args),
            Self::Propose(args) => {
                let bytes = read_input(env, &args.file)?;
                let wire = parse_proposal(&bytes, args.title).map_err(|error| {
                    CliError::Invalid(format!("{}: {error}", args.file.display()))
                })?;
                Invocation::new(name, &wire)
            }
            Self::Rule(RuleCommand::Add(args)) => Invocation::new(name, &args),
            Self::Send(args) => Invocation::new(name, &args),
            Self::Export(mut args) => {
                args.to = cwd(env)?.join(&args.to);
                Invocation::new(name, &args)
            }
            Self::Import(mut args) => {
                args.path = cwd(env)?.join(&args.path);
                Invocation::new(name, &args)
            }
            Self::Account(AccountCommand::List(args)) => Invocation::new(name, &args),
            Self::Account(AccountCommand::Add(args)) => {
                let password = if args.oauth.is_some() {
                    None
                } else {
                    Some(password(env, &args)?)
                };
                let wire = AccountAddArgs {
                    email: Sensitive::new(args.email),
                    name: args.name,
                    oauth: args.oauth,
                    host: args.host,
                    port: args.port,
                    security: args.security,
                    username: args.username.map(Sensitive::new),
                    password,
                    smtp_host: args.smtp_host,
                    smtp_port: args.smtp_port,
                    smtp_security: args.smtp_security,
                };
                Invocation::new(name, &wire)
            }
            Self::Account(AccountCommand::Remove(args)) => Invocation::new(name, &args),
            Self::Sync(args) => Invocation::new(name, &args),
        }
    }
}

/// The password for `account add`: the first line of stdin with `--password-stdin`,
/// else a prompt on the terminal without echo.
fn password(env: &dyn ClientEnv, args: &AccountAddCliArgs) -> Result<Sensitive<String>, CliError> {
    let password = if args.password_stdin {
        let bytes = zeroize::Zeroizing::new(env.read_stdin().map_err(|source| CliError::Read {
            path: PathBuf::from("-"),
            source,
        })?);
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| CliError::Invalid("the password on stdin is not UTF-8".into()))?;
        text.lines().next().unwrap_or_default().to_owned()
    } else {
        env.read_password(&format!("Password for {}: ", args.email))
            .map_err(|source| CliError::Read {
                path: PathBuf::from("/dev/tty"),
                source,
            })?
    };
    if password.is_empty() {
        return Err(CliError::Invalid("no password given".into()));
    }
    Ok(Sensitive::new(password))
}

fn cwd(env: &dyn ClientEnv) -> Result<PathBuf, CliError> {
    env.cwd().map_err(|source| CliError::Read {
        path: PathBuf::from("."),
        source,
    })
}

/// Reads a file, or stdin for `-`.
fn read_input(env: &dyn ClientEnv, path: &Path) -> Result<Vec<u8>, CliError> {
    let result = if path == Path::new("-") {
        env.read_stdin()
    } else {
        env.read_file(&cwd(env)?.join(path))
    };
    result.map_err(|source| CliError::Read {
        path: path.to_owned(),
        source,
    })
}

fn body(
    env: &dyn ClientEnv,
    file: Option<&Path>,
    inline: Option<Sensitive<String>>,
) -> Result<Option<Sensitive<String>>, CliError> {
    let Some(file) = file else {
        return Ok(inline);
    };
    let bytes = read_input(env, file)?;
    let text = String::from_utf8(bytes)
        .map_err(|_| CliError::Invalid(format!("{} is not UTF-8 text", file.display())))?;
    Ok(Some(Sensitive::new(text)))
}

fn attachments(env: &dyn ClientEnv, files: &[PathBuf]) -> Result<Vec<AttachmentUpload>, CliError> {
    files
        .iter()
        .map(|file| {
            let bytes = read_input(env, file)?;
            let name = file.file_name().map_or_else(
                || "attachment".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            Ok(AttachmentUpload {
                name: Sensitive::new(name),
                content_type: None,
                data: Sensitive::new(base64::engine::general_purpose::STANDARD.encode(bytes)),
            })
        })
        .collect()
}

/// A proposal file: an array of actions or `{ "title": …, "actions": [...] }`. A
/// `--title` on the command line wins.
fn parse_proposal(bytes: &[u8], title: Option<String>) -> Result<ProposeArgs, String> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    let mut args = if value.is_array() {
        ProposeArgs {
            title: None,
            actions: super::command::parse_args(value).map_err(|error| error.0)?,
        }
    } else {
        super::command::parse_args::<ProposeArgs>(value).map_err(|error| error.0)?
    };
    if title.is_some() {
        args.title = title;
    }
    if args.actions.is_empty() {
        return Err("no actions".into());
    }
    Ok(args)
}

/// A file name that stays inside the target directory: no separators, no leading dots,
/// no control characters.
fn sanitize_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .to_owned();
    if cleaned.is_empty() {
        "attachment".to_owned()
    } else {
        cleaned
    }
}

/// `dir/name`, or `dir/stem (2).ext` and so on if that exists.
fn unique_path(env: &dyn ClientEnv, dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !env.exists(&candidate) {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (name, None),
    };
    (2..)
        .map(|n| match ext {
            Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
            None => dir.join(format!("{stem} ({n})")),
        })
        .find(|path| !env.exists(path))
        .unwrap_or(candidate)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use clap::FromArgMatches as _;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct FakeEnv {
        files: RefCell<BTreeMap<PathBuf, Vec<u8>>>,
        stdin: Vec<u8>,
        prompts: RefCell<Vec<String>>,
    }

    impl ClientEnv for FakeEnv {
        fn cwd(&self) -> io::Result<PathBuf> {
            Ok(PathBuf::from("/work"))
        }
        fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
            self.files
                .borrow()
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }
        fn read_stdin(&self) -> io::Result<Vec<u8>> {
            Ok(self.stdin.clone())
        }
        fn exists(&self, path: &Path) -> bool {
            self.files.borrow().contains_key(path)
        }
        fn write_new_file(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
            let mut files = self.files.borrow_mut();
            if files.contains_key(path) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            files.insert(path.to_owned(), bytes.to_vec());
            Ok(())
        }
        fn read_password(&self, prompt: &str) -> io::Result<String> {
            self.prompts.borrow_mut().push(prompt.to_owned());
            Ok("from-tty".to_owned())
        }
    }

    fn parse(args: &[&str]) -> CliCommand {
        let matches = cli_command()
            .try_get_matches_from(std::iter::once("mazemail").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{args:?}: {error}"));
        CliCommand::from_arg_matches(&matches).unwrap()
    }

    fn invoke(args: &[&str], env: &FakeEnv) -> Invocation {
        parse(args).into_invocation(env).unwrap()
    }

    fn leaf_paths(cmd: &clap::Command, prefix: &mut Vec<String>, out: &mut BTreeSet<String>) {
        for sub in cmd.get_subcommands() {
            prefix.push(sub.get_name().to_owned());
            if sub.has_subcommands() {
                leaf_paths(sub, prefix, out);
            } else {
                out.insert(prefix.join(" "));
            }
            prefix.pop();
        }
    }

    #[test]
    fn cli_tree_matches_registry() {
        let mut leaves = BTreeSet::new();
        leaf_paths(&cli_command(), &mut Vec::new(), &mut leaves);
        let specs: BTreeSet<String> = registry().iter().map(CommandSpec::cli_name).collect();
        assert_eq!(leaves, specs);
        cli_command().debug_assert();
    }

    #[test]
    fn help_text_comes_from_the_registry() {
        let cmd = cli_command();
        let search = cmd.find_subcommand("search").unwrap();
        assert_eq!(search.get_about().unwrap().to_string(), "Search mail");
        let help = local_help(&["draft".into(), "new".into()]).unwrap();
        assert!(help.contains("Write a new draft"), "{help}");
        assert!(help.contains("--body-file"), "{help}");
        assert!(help.contains("Examples:"), "{help}");
        assert!(local_help(&["agents".into()]).unwrap().starts_with("# "));
        assert_eq!(local_help(&["nope".into()]), None);
    }

    #[test]
    fn every_invocation_targets_its_spec() {
        let env = FakeEnv::default();
        for (args, name) in [
            (&["status"][..], "status"),
            (&["login", "--profile", "triage", "--ttl", "1h"], "login"),
            (&["token", "create"], "token.create"),
            (&["token", "list"], "token.list"),
            (&["token", "revoke", "tk_abc"], "token.revoke"),
            (&["show", "t_b", "--format", "raw"], "show"),
            (&["folders", "--account", "a_b"], "folders"),
            (&["tags"], "tags"),
            (&["contacts", "anna"], "contacts"),
            (&["identities"], "identities"),
            (&["archive", "m_b", "t_c"], "archive"),
            (&["move", "--to", "f_b", "m_b"], "move"),
            (&["tag", "--add", "x", "m_b"], "tag"),
            (&["flag", "--clear", "m_b"], "flag"),
            (&["read", "--unread", "m_b"], "read"),
            (
                &[
                    "rule", "add", "--name", "n", "--when", "from:x", "--then", "archive",
                ],
                "rule.add",
            ),
            (&["send", "d_b"], "send"),
            (&["import", "mail.mbox"], "import"),
            (&["account", "list"], "account.list"),
            (&["account", "add", "a@example.org"], "account.add"),
            (
                &["account", "add", "b@gmail.com", "--oauth", "google"],
                "account.add",
            ),
            (&["account", "remove", "a_b"], "account.remove"),
            (&["sync"], "sync"),
            (&["sync", "--account", "a_b"], "sync"),
        ] {
            let invocation = invoke(args, &env);
            assert_eq!(invocation.cmd(), name, "{args:?}");
            // What the CLI sends always parses on the server side.
            if invocation.spec.is_builtin() {
                continue;
            }
            super::super::Command::from_wire(name, invocation.args.clone())
                .unwrap_or_else(|error| panic!("{args:?}: {error}"))
                .unwrap();
        }
    }

    #[test]
    fn search_ids_is_client_side() {
        let invocation = invoke(
            &["search", "from:anna", "--ids", "--limit", "5"],
            &FakeEnv::default(),
        );
        assert_eq!(invocation.args, json!({ "query": "from:anna", "limit": 5 }));
        assert!(invocation.render.ids_only);
    }

    #[test]
    fn usage_errors() {
        let cmd = cli_command();
        for args in [
            &["archive"][..],
            &["tag", "m_b"],
            &["move", "m_b"],
            &["move", "--to", "m_b", "m_c"],
            &["login", "--ttl", "forever"],
            &["login", "--profile", "admin"],
            &["draft", "new", "--body", "x", "--body-file", "y"],
            &[
                "rule", "add", "--name", "n", "--when", "q", "--then", "delete",
            ],
            &["token"],
        ] {
            let result = cmd
                .clone()
                .try_get_matches_from(std::iter::once("mazemail").chain(args.iter().copied()));
            assert!(result.is_err(), "{args:?} should fail");
        }
    }

    #[test]
    fn draft_reads_body_and_attachments_locally() {
        let env = FakeEnv {
            stdin: b"Hello Anna".to_vec(),
            ..FakeEnv::default()
        };
        env.files
            .borrow_mut()
            .insert("/work/notes/a.pdf".into(), vec![1, 2, 3]);
        let invocation = invoke(
            &[
                "draft",
                "new",
                "--to",
                "anna@example.com",
                "--subject",
                "Hi",
                "--body-file",
                "-",
                "--attach",
                "notes/a.pdf",
            ],
            &env,
        );
        assert_eq!(
            invocation.args,
            json!({
                "reply_to": null, "reply_all": false, "forward": null, "identity": null,
                "to": ["anna@example.com"], "cc": [], "bcc": [], "subject": "Hi",
                "body": "Hello Anna",
                "attachments": [{ "name": "a.pdf", "content_type": null, "data": "AQID" }]
            })
        );
        let error = parse(&["draft", "new", "--body-file", "missing.md"])
            .into_invocation(&env)
            .unwrap_err();
        assert_eq!(error.exit_code(), ExitCode::Usage);
    }

    #[test]
    fn draft_edit_only_sends_changes() {
        let invocation = invoke(
            &["draft", "edit", "d_b", "--subject", "New"],
            &FakeEnv::default(),
        );
        let args: DraftEditArgs = serde_json::from_value(invocation.args).unwrap();
        assert_eq!(args.to, None);
        assert_eq!(args.subject.unwrap().expose(), "New");
        assert_eq!(args.body, None);
    }

    #[test]
    fn propose_file_forms() {
        let env = FakeEnv::default();
        env.files.borrow_mut().insert(
            "/work/a.json".into(),
            br#"[{ "action": "archive", "ids": ["m_b"] }]"#.to_vec(),
        );
        env.files.borrow_mut().insert(
            "/work/b.json".into(),
            br#"{ "title": "Triage", "actions": [{ "action": "tag", "ids": ["m_b"], "add": ["x"] }] }"#.to_vec(),
        );
        env.files
            .borrow_mut()
            .insert("/work/c.json".into(), b"[]".to_vec());
        env.files.borrow_mut().insert(
            "/work/d.json".into(),
            br#"[{ "action": "delete", "ids": ["m_b"] }]"#.to_vec(),
        );

        let a = invoke(&["propose", "--file", "a.json", "--title", "T"], &env);
        assert_eq!(a.args["title"], "T");
        let b = invoke(&["propose", "--file", "b.json"], &env);
        assert_eq!(b.args["title"], "Triage");
        for bad in ["c.json", "d.json", "missing.json"] {
            let error = parse(&["propose", "--file", bad])
                .into_invocation(&env)
                .unwrap_err();
            assert_eq!(error.exit_code(), ExitCode::Usage, "{bad}: {error}");
        }
    }

    #[test]
    fn paths_are_made_absolute() {
        let env = FakeEnv::default();
        let export = invoke(&["export", "in:inbox", "--to", "out.mbox"], &env);
        assert_eq!(export.args["to"], "/work/out.mbox");
        let import = invoke(&["import", "/abs/mail"], &env);
        assert_eq!(import.args["path"], "/abs/mail");
    }

    #[test]
    fn attachment_save_writes_locally_without_overwriting() {
        let env = FakeEnv::default();
        env.files
            .borrow_mut()
            .insert("/work/out/report.pdf".into(), vec![0]);
        let invocation = invoke(&["attachment", "save", "m_b.2", "--to", "out"], &env);
        assert_eq!(invocation.args, json!({ "id": "m_b.2" }));
        let data = json!({
            "id": "m_b.2", "filename": "../../report.pdf", "content_type": "application/pdf",
            "size": 3, "data": "AQID"
        });
        let printed = invocation.finish(data, &env).unwrap();
        assert_eq!(printed["path"], "/work/out/report (2).pdf");
        assert_eq!(printed["size"], 3);
        assert_eq!(
            env.files.borrow()[Path::new("/work/out/report (2).pdf")],
            vec![1, 2, 3]
        );

        let bad =
            json!({ "id": "x", "filename": "a", "content_type": "t", "size": 1, "data": "!!" });
        assert_eq!(
            invocation.finish(bad, &env).unwrap_err().exit_code(),
            ExitCode::Failure
        );
    }

    #[test]
    fn file_name_sanitizing() {
        assert_eq!(sanitize_file_name("a.pdf"), "a.pdf");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("C:\\x\\y.txt"), "y.txt");
        assert_eq!(sanitize_file_name(".bashrc"), "bashrc");
        assert_eq!(sanitize_file_name(".."), "attachment");
        assert_eq!(sanitize_file_name("a\u{0}b\n.txt"), "ab.txt");
        assert_eq!(sanitize_file_name(""), "attachment");
    }
}

#[cfg(test)]
mod account_tests {
    use std::cell::RefCell;

    use clap::FromArgMatches as _;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct Env {
        stdin: Vec<u8>,
        prompts: RefCell<Vec<String>>,
    }

    impl ClientEnv for Env {
        fn cwd(&self) -> io::Result<PathBuf> {
            Ok(PathBuf::from("/work"))
        }
        fn read_file(&self, _: &Path) -> io::Result<Vec<u8>> {
            Err(io::ErrorKind::NotFound.into())
        }
        fn read_stdin(&self) -> io::Result<Vec<u8>> {
            Ok(self.stdin.clone())
        }
        fn exists(&self, _: &Path) -> bool {
            false
        }
        fn write_new_file(&self, _: &Path, _: &[u8]) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
        fn read_password(&self, prompt: &str) -> io::Result<String> {
            self.prompts.borrow_mut().push(prompt.to_owned());
            Ok("typed secret".to_owned())
        }
    }

    fn invoke(args: &[&str], env: &Env) -> Result<Invocation, CliError> {
        let matches = cli_command()
            .try_get_matches_from(std::iter::once("mazemail").chain(args.iter().copied()))
            .unwrap_or_else(|error| panic!("{args:?}: {error}"));
        CliCommand::from_arg_matches(&matches)
            .unwrap()
            .into_invocation(env)
    }

    #[test]
    fn the_password_comes_from_the_terminal_or_stdin_never_an_argument() {
        let env = Env::default();
        let typed = invoke(
            &[
                "account",
                "add",
                "alice@example.org",
                "--host",
                "imap.example.org",
            ],
            &env,
        )
        .unwrap();
        assert_eq!(typed.args["password"], "typed secret");
        assert_eq!(typed.args["host"], "imap.example.org");
        assert_eq!(*env.prompts.borrow(), ["Password for alice@example.org: "]);

        let piped = Env {
            stdin: b"from stdin\nignored\n".to_vec(),
            ..Env::default()
        };
        let invocation = invoke(
            &[
                "account",
                "add",
                "alice@example.org",
                "--password-stdin",
                "--security",
                "none",
                "--port",
                "1143",
            ],
            &piped,
        )
        .unwrap();
        assert_eq!(invocation.args["password"], "from stdin");
        assert_eq!(invocation.args["security"], "none");
        assert_eq!(invocation.args["port"], 1143);
        assert!(piped.prompts.borrow().is_empty());
        // What the CLI sends parses on the server side.
        super::super::Command::from_wire("account.add", invocation.args)
            .unwrap()
            .unwrap();

        let empty = Env {
            stdin: b"\n".to_vec(),
            ..Env::default()
        };
        assert!(invoke(&["account", "add", "a@b.c", "--password-stdin"], &empty).is_err());

        // There is no password option at all.
        assert!(
            cli_command()
                .try_get_matches_from(["mazemail", "account", "add", "a@b.c", "--password", "x"])
                .is_err()
        );
        // A browser sign-in takes no password and no server options.
        let oauth = invoke(
            &["account", "add", "b@gmail.com", "--oauth", "google"],
            &env,
        )
        .unwrap();
        assert_eq!(oauth.args["oauth"], "google");
        assert_eq!(oauth.args["password"], json!(null));
        assert!(
            cli_command()
                .try_get_matches_from([
                    "mazemail",
                    "account",
                    "add",
                    "b@gmail.com",
                    "--oauth",
                    "google",
                    "--password-stdin"
                ])
                .is_err()
        );
    }

    #[test]
    fn the_server_requires_a_password_without_oauth() {
        let error = super::super::Command::from_wire("account.add", json!({ "email": "a@b.c" }))
            .unwrap_err();
        assert!(error.0.contains("password"), "{error}");
        let error = super::super::Command::from_wire(
            "account.add",
            json!({ "email": "a@b.c", "oauth": "google", "password": "x" }),
        )
        .unwrap_err();
        assert!(error.0.contains("oauth"), "{error}");
    }
}

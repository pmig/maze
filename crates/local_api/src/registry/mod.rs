//! The command registry: every `mazemail` command defined once (PLAN §4.11).
//!
//! Each [`CommandSpec`] holds the wire name, the CLI path, help text, examples, the
//! [`Permission`], the JSON Schemas of arguments and output, and how to print the
//! output for people. The same registry drives:
//!
//! - the server's dispatcher (permissions, argument parsing into [`Command`]),
//! - the CLI ([`cli::augment`] builds the clap tree, [`cli::CliCommand`] turns parsed
//!   arguments into a request, [`render::render_human`] prints tables),
//! - AGENTS.md and `mazemail help agents` ([`agents_md`]).

mod agents_md;
pub mod args;
pub mod cli;
mod command;
pub mod output;
pub mod render;
mod shape;

use schemars::{JsonSchema, Schema};

pub use agents_md::agents_md;
pub(crate) use command::parse_args as parse_wire_args;
pub use command::{ArgsError, Command};

use crate::permission::{Access, Permission, access};
use crate::token::Profile;
use args::*;
use output::*;

/// One command.
#[derive(Debug, Clone, Copy)]
pub struct CommandSpec {
    /// Wire name (`search`, `draft.new`).
    pub name: &'static str,
    /// Words after `mazemail` (`["draft", "new"]`).
    pub path: &'static [&'static str],
    /// One-line summary (clap `about`).
    pub summary: &'static str,
    /// Longer help (clap `long_about`, AGENTS.md).
    pub help: &'static str,
    /// Permission class.
    pub permission: Permission,
    /// Whether a token is needed. Only `help`, `status`, `login` and `token create`
    /// work without one.
    pub requires_token: bool,
    /// Whether the command reads or changes mail (and so needs at least one account in
    /// the token's effective scope).
    pub touches_mail: bool,
    /// Example invocations.
    pub examples: &'static [Example],
    /// JSON Schema of the wire arguments.
    pub args_schema: fn() -> Schema,
    /// JSON Schema of what `--json` prints.
    pub output_schema: fn() -> Schema,
    /// How the CLI prints the output without `--json`.
    pub human: Human,
}

/// An example invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Example {
    /// The shell command.
    pub command: &'static str,
    /// What it does.
    pub description: &'static str,
}

/// How the CLI prints a command's output for people.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Human {
    /// An array of objects as a table.
    Table(&'static [Column]),
    /// One string field of an object, verbatim.
    Field(&'static str),
    /// An object as `key: value` lines.
    Fields,
}

/// A table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    /// Header.
    pub header: &'static str,
    /// Field path in each row (`id`, `scope.accounts`).
    pub field: &'static str,
}

impl CommandSpec {
    /// The command as typed after `mazemail` (`draft new`).
    pub fn cli_name(&self) -> String {
        self.path.join(" ")
    }

    /// Whether the local API answers the command itself instead of `mazemail_core`.
    pub fn is_builtin(&self) -> bool {
        !Command::NAMES.contains(&self.name)
    }

    /// What a token with `profile` gets for this command.
    ///
    /// Tokenless commands are always allowed, except that `login` and `token create`
    /// always ask (tokens can't mint tokens). Revoking the caller's own token is
    /// always allowed (not reflected here).
    pub fn access(&self, profile: Profile, allow_send: bool) -> Access {
        if !self.requires_token {
            return if self.permission == Permission::Ask {
                Access::Ask
            } else {
                Access::Allow
            };
        }
        access(profile, self.permission, allow_send)
    }
}

fn schema<T: JsonSchema>() -> Schema {
    schemars::schema_for!(T)
}

const fn ex(command: &'static str, description: &'static str) -> Example {
    Example {
        command,
        description,
    }
}

const fn col(header: &'static str, field: &'static str) -> Column {
    Column { header, field }
}

const MUTATION_HELP_SUFFIX: &str = "Under the triage profile this becomes a proposal that you accept \
or reject in Maze's review UI; under the writer profile it runs directly.";

static REGISTRY: &[CommandSpec] = &[
    CommandSpec {
        name: "status",
        path: &["status"],
        summary: "Show whether Maze is running and whether you are signed in",
        help: "Shows Maze's version and protocol, and the current token's profile, scope and \
               expiry. Works without a token.",
        permission: Permission::Read,
        requires_token: false,
        touches_mail: false,
        examples: &[ex(
            "mazemail status --json | jq .signed_in",
            "Check the session",
        )],
        args_schema: schema::<StatusArgs>,
        output_schema: schema::<StatusOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "help",
        path: &["help"],
        summary: "List commands and what your token may do with them",
        help: "Without a topic, lists every command with its permission and what happens with \
               your token (allow, propose, ask or deny). `mazemail help <command>` shows a \
               command's help, `mazemail help agents` prints this AGENTS.md. Works without a \
               token.",
        permission: Permission::Read,
        requires_token: false,
        touches_mail: false,
        examples: &[
            ex("mazemail help draft new", "Help for one command"),
            ex("mazemail help agents > AGENTS.md", "Write the agent guide"),
        ],
        args_schema: schema::<HelpArgs>,
        output_schema: schema::<HelpOutput>,
        human: Human::Table(&[
            col("COMMAND", "command"),
            col("ACCESS", "access"),
            col("SUMMARY", "summary"),
        ]),
    },
    CommandSpec {
        name: "login",
        path: &["login"],
        summary: "Ask Maze for a session token",
        help: "Maze shows which program is asking, the profile, scope and lifetime, and waits \
               for your approval. Approved, it prints `export MAZEMAIL_SESSION=…` for the \
               current shell. A program that already holds a token still needs approval, and \
               the new token gets exactly what you approve. Accounts whose AI policy excludes \
               agents are never in a token's scope.",
        permission: Permission::Ask,
        requires_token: false,
        touches_mail: false,
        examples: &[
            ex(
                "eval \"$(mazemail login --profile read-only --ttl 8h)\"",
                "Sign in the current shell",
            ),
            ex(
                "mazemail login --profile triage --account a_b --label claude",
                "A triage token for one account",
            ),
        ],
        args_schema: schema::<LoginArgs>,
        output_schema: schema::<LoginOutput>,
        human: Human::Field("export"),
    },
    CommandSpec {
        name: "token.create",
        path: &["token", "create"],
        summary: "Ask Maze for a session token (same as `mazemail login`)",
        help: "An alias of `mazemail login`.",
        permission: Permission::Ask,
        requires_token: false,
        touches_mail: false,
        examples: &[ex(
            "mazemail token create --profile read-only --ttl 1h --json | jq -r .token",
            "Print just the token",
        )],
        args_schema: schema::<LoginArgs>,
        output_schema: schema::<LoginOutput>,
        human: Human::Field("export"),
    },
    CommandSpec {
        name: "token.list",
        path: &["token", "list"],
        summary: "List API tokens",
        help: "Lists every token with its profile, scope, expiry and creator. Secrets are never \
               shown; Maze only keeps hashes.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: false,
        examples: &[ex("mazemail token list", "Show tokens")],
        args_schema: schema::<TokenListArgs>,
        output_schema: schema::<Vec<crate::token::TokenInfo>>,
        human: Human::Table(&[
            col("ID", "id"),
            col("LABEL", "label"),
            col("PROFILE", "profile"),
            col("EXPIRES", "expires_at"),
            col("REVOKED", "revoked"),
        ]),
    },
    CommandSpec {
        name: "token.revoke",
        path: &["token", "revoke"],
        summary: "Revoke an API token",
        help: "Revoking your own token (the one in MAZEMAIL_SESSION) always works. Revoking \
               another token asks for approval in Maze.",
        permission: Permission::Ask,
        requires_token: true,
        touches_mail: false,
        examples: &[ex(
            "mazemail token revoke tk_q3x7k2mz4b5ca",
            "Revoke a token",
        )],
        args_schema: schema::<TokenRevokeArgs>,
        output_schema: schema::<RevokeOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "search",
        path: &["search"],
        summary: "Search mail",
        help: "Searches every account and folder in your token's scope, newest first. The query \
               language is Thunderbird's quick filter plus Gmail-style operators.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: true,
        examples: &[
            ex(
                "mazemail search 'from:anna has:attachment after:2026-09-01' --json | jq -r '.[0].thread'",
                "Thread of the newest match",
            ),
            ex(
                "mazemail search 'is:unread' --limit 10",
                "Ten unread messages",
            ),
        ],
        args_schema: schema::<SearchArgs>,
        output_schema: schema::<Vec<SearchHit>>,
        human: Human::Table(&[
            col("ID", "id"),
            col("DATE", "date"),
            col("FROM", "from"),
            col("SUBJECT", "subject"),
        ]),
    },
    CommandSpec {
        name: "show",
        path: &["show"],
        summary: "Show a message or a whole thread",
        help: "Renders a message (`m_…`) or a thread (`t_…`, all messages oldest first) as \
               Markdown, plain text or the raw message. Remote content is never loaded.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: true,
        examples: &[
            ex("mazemail show t_19bd --format md", "A thread as Markdown"),
            ex(
                "mazemail show m_8f3a2c --format raw > message.eml",
                "Save the raw message",
            ),
        ],
        args_schema: schema::<ShowArgs>,
        output_schema: schema::<ShowOutput>,
        human: Human::Field("content"),
    },
    CommandSpec {
        name: "folders",
        path: &["folders"],
        summary: "List folders",
        help: "Lists the folders in your token's scope with unread and total counts.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail folders --json | jq -r '.[] | select(.role == \"archive\") | .id'",
            "Find the archive folders",
        )],
        args_schema: schema::<FoldersArgs>,
        output_schema: schema::<Vec<FolderInfo>>,
        human: Human::Table(&[
            col("ID", "id"),
            col("ACCOUNT", "account"),
            col("PATH", "path"),
            col("UNREAD", "unread"),
            col("TOTAL", "total"),
        ]),
    },
    CommandSpec {
        name: "tags",
        path: &["tags"],
        summary: "List tags",
        help: "Lists tags (IMAP keywords) with their colors.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: true,
        examples: &[ex("mazemail tags", "Show tags")],
        args_schema: schema::<TagsArgs>,
        output_schema: schema::<Vec<TagInfo>>,
        human: Human::Table(&[col("ID", "id"), col("NAME", "name"), col("COLOR", "color")]),
    },
    CommandSpec {
        name: "contacts",
        path: &["contacts"],
        summary: "Find contacts",
        help: "Finds people you have exchanged mail with by name or address.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: true,
        examples: &[ex("mazemail contacts anna", "Find Anna's address")],
        args_schema: schema::<ContactsArgs>,
        output_schema: schema::<Vec<ContactInfo>>,
        human: Human::Table(&[col("NAME", "name"), col("EMAIL", "email")]),
    },
    CommandSpec {
        name: "identities",
        path: &["identities"],
        summary: "List your sending identities",
        help: "Lists the identities (From addresses) of the accounts in scope. `draft new` picks \
               one automatically when you don't pass --identity.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: true,
        examples: &[ex("mazemail identities", "Show identities")],
        args_schema: schema::<IdentitiesArgs>,
        output_schema: schema::<Vec<IdentityInfo>>,
        human: Human::Table(&[
            col("ID", "id"),
            col("NAME", "name"),
            col("EMAIL", "email"),
            col("DEFAULT", "default"),
        ]),
    },
    CommandSpec {
        name: "attachment.save",
        path: &["attachment", "save"],
        summary: "Save an attachment to a file",
        help: "Writes the attachment into --to (default: the current directory) under its \
               sanitized file name, never overwriting an existing file.",
        permission: Permission::Read,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail attachment save m_8f3a2c.2 --to .",
            "Save into the current directory",
        )],
        args_schema: schema::<AttachmentSaveArgs>,
        output_schema: schema::<AttachmentSaved>,
        human: Human::Field("path"),
    },
    CommandSpec {
        name: "draft.new",
        path: &["draft", "new"],
        summary: "Write a new draft, reply or forward",
        help: "Creates a draft in Maze for you to review and send. Drafts are harmless: nothing \
               is sent. The identity is picked automatically from the recipients and the \
               replied-to message unless you pass --identity.",
        permission: Permission::Draft,
        requires_token: true,
        touches_mail: true,
        examples: &[
            ex(
                "mazemail draft new --reply-to m_8f3a2c --body-file reply.md",
                "Reply; identity picked automatically",
            ),
            ex(
                "echo 'See you then.' | mazemail draft new --to anna@example.com --subject Friday --body-file -",
                "New message with the body from stdin",
            ),
        ],
        args_schema: schema::<DraftNewArgs>,
        output_schema: schema::<DraftOutput>,
        human: Human::Field("id"),
    },
    CommandSpec {
        name: "draft.edit",
        path: &["draft", "edit"],
        summary: "Change a draft",
        help: "Replaces the given fields of a draft; everything else stays as it is.",
        permission: Permission::Draft,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail draft edit d_42 --subject 'Re: Friday' --body-file reply.md",
            "Replace subject and body",
        )],
        args_schema: schema::<DraftEditArgs>,
        output_schema: schema::<DraftOutput>,
        human: Human::Field("id"),
    },
    CommandSpec {
        name: "archive",
        path: &["archive"],
        summary: "Archive messages or threads",
        help: MUTATION_HELP_SUFFIX,
        permission: Permission::Propose,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail archive $(mazemail search 'list:news older_than:7d' --ids)",
            "Archive old newsletters (a proposal under triage)",
        )],
        args_schema: schema::<ArchiveArgs>,
        output_schema: schema::<MutationOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "move",
        path: &["move"],
        summary: "Move messages or threads to a folder",
        help: MUTATION_HELP_SUFFIX,
        permission: Permission::Propose,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail move --to f_k2 m_8f3a2c t_19bd",
            "Move to a folder",
        )],
        args_schema: schema::<MoveArgs>,
        output_schema: schema::<MutationOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "tag",
        path: &["tag"],
        summary: "Add or remove tags",
        help: MUTATION_HELP_SUFFIX,
        permission: Permission::Propose,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail tag --add 'To Do' --remove Later m_8f3a2c",
            "Retag a message",
        )],
        args_schema: schema::<TagArgs>,
        output_schema: schema::<MutationOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "flag",
        path: &["flag"],
        summary: "Flag or unflag messages",
        help: MUTATION_HELP_SUFFIX,
        permission: Permission::Propose,
        requires_token: true,
        touches_mail: true,
        examples: &[ex("mazemail flag m_8f3a2c", "Flag a message")],
        args_schema: schema::<FlagArgs>,
        output_schema: schema::<MutationOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "read",
        path: &["read"],
        summary: "Mark messages read or unread",
        help: MUTATION_HELP_SUFFIX,
        permission: Permission::Propose,
        requires_token: true,
        touches_mail: true,
        examples: &[ex("mazemail read --unread t_19bd", "Mark a thread unread")],
        args_schema: schema::<ReadArgs>,
        output_schema: schema::<MutationOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "propose",
        path: &["propose"],
        summary: "Propose a batch of actions for review",
        help: "Sends a list of actions (archive, move, tag, flag, mark_read, unsubscribe) that \
               shows up grouped in Maze's review UI, where you accept or reject each item. \
               Nothing changes until you accept.",
        permission: Permission::Draft,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail propose --title 'Inbox triage' --file actions.json",
            "Propose actions from a file",
        )],
        args_schema: schema::<ProposeArgs>,
        output_schema: schema::<ProposalOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "rule.add",
        path: &["rule", "add"],
        summary: "Add a filter rule",
        help: "Adds a rule that runs on new mail. Always asks for approval in Maze.",
        permission: Permission::Ask,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail rule add --name Receipts --when 'from:receipts@shop.example' --then tag:Receipts --then archive",
            "Tag and archive receipts",
        )],
        args_schema: schema::<RuleAddArgs>,
        output_schema: schema::<RuleOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "send",
        path: &["send"],
        summary: "Send a draft",
        help: "Disabled by default. A token approved with --allow-send may use it, and then \
               every send waits for your approval in Maze (\"waiting for approval in Maze…\") \
               and is never approved automatically.",
        permission: Permission::Disabled,
        requires_token: true,
        touches_mail: true,
        examples: &[ex("mazemail send d_42", "Send after approval")],
        args_schema: schema::<SendArgs>,
        output_schema: schema::<SendOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "export",
        path: &["export"],
        summary: "Export messages to mbox or Maildir",
        help: "Writes the matching messages as plaintext to --to. Always asks for approval in \
               Maze, because the export leaves encrypted storage.",
        permission: Permission::Ask,
        requires_token: true,
        touches_mail: true,
        examples: &[ex(
            "mazemail export 'folder:Inbox after:2026-01-01' --to inbox.mbox",
            "Export this year's inbox",
        )],
        args_schema: schema::<ExportArgs>,
        output_schema: schema::<ExportOutput>,
        human: Human::Fields,
    },
    CommandSpec {
        name: "import",
        path: &["import"],
        summary: "Import an mbox file, a Maildir or a Thunderbird profile",
        help: "Imports into Local Folders or --account. Always asks for approval in Maze.",
        permission: Permission::Ask,
        requires_token: true,
        touches_mail: true,
        examples: &[ex("mazemail import ~/old-mail.mbox", "Import an mbox file")],
        args_schema: schema::<ImportArgs>,
        output_schema: schema::<ImportOutput>,
        human: Human::Fields,
    },
];

/// Every command, in help order.
pub fn registry() -> &'static [CommandSpec] {
    REGISTRY
}

/// The command with wire name `name`.
pub fn find(name: &str) -> Option<&'static CommandSpec> {
    REGISTRY.iter().find(|spec| spec.name == name)
}

/// The command with CLI path `path` (`["draft", "new"]`).
pub fn find_by_path(path: &[&str]) -> Option<&'static CommandSpec> {
    REGISTRY.iter().find(|spec| spec.path == path)
}

/// Command groups with subcommands, and their summaries.
pub(crate) const GROUPS: &[(&str, &str)] = &[
    ("token", "Create, list and revoke API tokens"),
    ("attachment", "Work with attachments"),
    ("draft", "Write and change drafts"),
    ("rule", "Manage filter rules"),
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn names_and_paths_are_unique_and_consistent() {
        let mut names = HashSet::new();
        let mut paths = HashSet::new();
        for spec in registry() {
            assert!(names.insert(spec.name), "duplicate {}", spec.name);
            assert!(paths.insert(spec.path), "duplicate {:?}", spec.path);
            assert_eq!(spec.name, spec.path.join("."));
            assert!(!spec.examples.is_empty(), "{} has no examples", spec.name);
            for example in spec.examples {
                assert!(
                    example.command.starts_with("mazemail ")
                        || example.command.contains("mazemail ")
                );
            }
            // Schemas generate.
            let _ = (spec.args_schema)();
            let _ = (spec.output_schema)();
        }
        assert_eq!(find("draft.new").unwrap().cli_name(), "draft new");
        assert_eq!(find_by_path(&["token", "list"]).unwrap().name, "token.list");
        assert!(find("delete").is_none());
    }

    #[test]
    fn concept_commands_are_present() {
        for name in [
            "status",
            "help",
            "login",
            "token.create",
            "token.list",
            "token.revoke",
            "search",
            "show",
            "folders",
            "tags",
            "contacts",
            "identities",
            "attachment.save",
            "draft.new",
            "draft.edit",
            "archive",
            "move",
            "tag",
            "flag",
            "read",
            "propose",
            "rule.add",
            "send",
            "export",
            "import",
        ] {
            assert!(find(name).is_some(), "{name} missing");
        }
    }

    #[test]
    fn builtins_and_mail_commands_partition_the_registry() {
        for spec in registry() {
            let is_mail = Command::NAMES.contains(&spec.name);
            assert_eq!(spec.is_builtin(), !is_mail, "{}", spec.name);
            // Mail commands need a token.
            if is_mail {
                assert!(spec.requires_token && spec.touches_mail, "{}", spec.name);
            }
        }
        for name in Command::NAMES {
            assert!(find(name).is_some(), "{name} not in the registry");
        }
    }

    #[test]
    fn only_help_status_and_login_are_tokenless() {
        let tokenless: Vec<_> = registry()
            .iter()
            .filter(|spec| !spec.requires_token)
            .map(|spec| spec.name)
            .collect();
        assert_eq!(tokenless, ["status", "help", "login", "token.create"]);
    }

    #[test]
    fn concept_triage_column() {
        use Access::*;
        let triage = |name: &str| find(name).unwrap().access(Profile::Triage, false);
        for name in [
            "search",
            "show",
            "folders",
            "tags",
            "contacts",
            "identities",
            "attachment.save",
        ] {
            assert_eq!(triage(name), Allow, "{name}");
        }
        for name in ["draft.new", "draft.edit", "propose"] {
            assert_eq!(triage(name), Allow, "{name}");
        }
        for name in ["archive", "move", "tag", "flag", "read"] {
            assert_eq!(triage(name), Propose, "{name}");
        }
        assert_eq!(triage("rule.add"), Ask);
        assert_eq!(triage("send"), Deny);
        assert_eq!(find("send").unwrap().access(Profile::Triage, true), Ask);
        assert_eq!(triage("login"), Ask);
        assert_eq!(find("login").unwrap().access(Profile::Writer, false), Ask);
        assert_eq!(triage("status"), Allow);
        // Read-only can only read.
        for spec in registry().iter().filter(|spec| spec.requires_token) {
            let expected = if spec.permission == Permission::Read {
                Allow
            } else {
                Deny
            };
            assert_eq!(
                spec.access(Profile::ReadOnly, true),
                expected,
                "{}",
                spec.name
            );
        }
    }
}

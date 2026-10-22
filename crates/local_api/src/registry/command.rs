//! The typed mail commands that [`CommandHandler`](crate::CommandHandler) receives.

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::args::{
    ArchiveArgs, AttachmentSaveArgs, ContactsArgs, DraftEditArgs, DraftNewArgs, ExportArgs,
    FlagArgs, FoldersArgs, IdentitiesArgs, ImportArgs, MoveArgs, ProposeArgs, ReadArgs,
    RuleAddArgs, SearchArgs, SendArgs, ShowArgs, TagArgs, TagsArgs,
};

/// Arguments that don't match a command's schema.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid arguments: {0}")]
pub struct ArgsError(pub String);

/// Deserializes `args`, naming the offending field on error.
pub(crate) fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ArgsError> {
    serde_path_to_error::deserialize(args).map_err(|error| {
        let path = error.path().to_string();
        let inner = error.into_inner();
        if path == "." {
            ArgsError(inner.to_string())
        } else {
            ArgsError(format!("{path}: {inner}"))
        }
    })
}

macro_rules! mail_commands {
    ($( $(#[$doc:meta])* $variant:ident($args:ty) = $name:literal, )*) => {
        /// A mail command with parsed arguments. The local API answers `help`, `status`,
        /// `login` and `token …` itself; everything else reaches `mazemail_core` as a
        /// `Command`.
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub enum Command {
            $( $(#[$doc])* $variant($args), )*
        }

        impl Command {
            /// Wire names of every mail command.
            pub const NAMES: &'static [&'static str] = &[$($name),*];

            /// The wire name.
            pub fn name(&self) -> &'static str {
                match self {
                    $( Self::$variant(_) => $name, )*
                }
            }

            /// Parses the arguments of the mail command `name`. `Ok(None)` if `name` is
            /// not a mail command.
            pub fn from_wire(name: &str, args: Value) -> Result<Option<Self>, ArgsError> {
                let command = match name {
                    $( $name => Self::$variant(parse_args(args)?), )*
                    _ => return Ok(None),
                };
                command.validate()?;
                Ok(Some(command))
            }
        }
    };
}

mail_commands! {
    /// `mazemail search`.
    Search(SearchArgs) = "search",
    /// `mazemail show`.
    Show(ShowArgs) = "show",
    /// `mazemail folders`.
    Folders(FoldersArgs) = "folders",
    /// `mazemail tags`.
    Tags(TagsArgs) = "tags",
    /// `mazemail contacts`.
    Contacts(ContactsArgs) = "contacts",
    /// `mazemail identities`.
    Identities(IdentitiesArgs) = "identities",
    /// `mazemail attachment save`. Return
    /// [`AttachmentData`](crate::registry::output::AttachmentData); the CLI writes the file.
    AttachmentSave(AttachmentSaveArgs) = "attachment.save",
    /// `mazemail draft new`.
    DraftNew(DraftNewArgs) = "draft.new",
    /// `mazemail draft edit`.
    DraftEdit(DraftEditArgs) = "draft.edit",
    /// `mazemail archive`. Honor [`ExecutionMode`](crate::ExecutionMode).
    Archive(ArchiveArgs) = "archive",
    /// `mazemail move`. Honor [`ExecutionMode`](crate::ExecutionMode).
    Move(MoveArgs) = "move",
    /// `mazemail tag`. Honor [`ExecutionMode`](crate::ExecutionMode).
    Tag(TagArgs) = "tag",
    /// `mazemail flag`. Honor [`ExecutionMode`](crate::ExecutionMode).
    Flag(FlagArgs) = "flag",
    /// `mazemail read`. Honor [`ExecutionMode`](crate::ExecutionMode).
    Read(ReadArgs) = "read",
    /// `mazemail propose`. Always creates a proposal.
    Propose(ProposeArgs) = "propose",
    /// `mazemail rule add`. Already approved by the user when it arrives.
    RuleAdd(RuleAddArgs) = "rule.add",
    /// `mazemail send`. Already approved by the user when it arrives.
    Send(SendArgs) = "send",
    /// `mazemail export`. Already approved by the user when it arrives.
    Export(ExportArgs) = "export",
    /// `mazemail import`. Already approved by the user when it arrives.
    Import(ImportArgs) = "import",
}

impl Command {
    /// Checks what the schema alone can't express.
    fn validate(&self) -> Result<(), ArgsError> {
        let ids = match self {
            Self::Archive(args) => Some(&args.ids),
            Self::Move(args) => Some(&args.ids),
            Self::Tag(args) => Some(&args.ids),
            Self::Flag(args) => Some(&args.ids),
            Self::Read(args) => Some(&args.ids),
            _ => None,
        };
        if ids.is_some_and(Vec::is_empty) {
            return Err(ArgsError("ids: at least one id is required".into()));
        }
        match self {
            Self::Tag(args) if args.add.is_empty() && args.remove.is_empty() => {
                Err(ArgsError("add or remove at least one tag".into()))
            }
            Self::Propose(args) if args.actions.is_empty() => {
                Err(ArgsError("actions: at least one action is required".into()))
            }
            Self::Propose(args) if args.actions.iter().any(|action| action.ids().is_empty()) => {
                Err(ArgsError(
                    "actions: every action needs at least one id".into(),
                ))
            }
            Self::RuleAdd(args) if args.actions.is_empty() => {
                Err(ArgsError("actions: at least one action is required".into()))
            }
            Self::Export(args) if !args.to.is_absolute() => {
                Err(ArgsError("to: must be an absolute path".into()))
            }
            Self::Import(args) if !args.path.is_absolute() => {
                Err(ArgsError("path: must be an absolute path".into()))
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_known_commands() {
        let command = Command::from_wire("search", json!({ "query": "from:anna" }))
            .unwrap()
            .unwrap();
        let Command::Search(args) = &command else {
            panic!("{command:?}");
        };
        assert_eq!(args.query.expose(), "from:anna");
        assert_eq!(args.limit, 50);
        assert_eq!(command.name(), "search");
        assert_eq!(Command::from_wire("login", json!({})).unwrap(), None);
    }

    #[test]
    fn reports_bad_arguments_with_path() {
        let error = Command::from_wire("search", json!({ "query": 1 })).unwrap_err();
        assert!(error.0.starts_with("query: invalid type"), "{error}");
        let error =
            Command::from_wire("search", json!({ "query": "x", "dry_run": true })).unwrap_err();
        assert!(error.0.contains("unknown field `dry_run`"), "{error}");
        let error = Command::from_wire("archive", json!({ "ids": [] })).unwrap_err();
        assert!(error.0.contains("at least one id"), "{error}");
        let error = Command::from_wire("tag", json!({ "ids": ["m_b"] })).unwrap_err();
        assert!(error.0.contains("at least one tag"), "{error}");
        let error = Command::from_wire("export", json!({ "query": "x", "to": "rel/out.mbox" }))
            .unwrap_err();
        assert!(error.0.contains("absolute"), "{error}");
        let error = Command::from_wire("move", json!({ "ids": ["m_b"], "to": "a_b" })).unwrap_err();
        assert!(error.0.starts_with("to: "), "{error}");
    }
}

//! Command permissions and the profile matrix (CONCEPT §10.2).

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::token::Profile;

/// The permission class of a command, set once in the [registry](crate::registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Reads mail or metadata: allowed for every profile.
    Read,
    /// Creates things you review before they have an effect (drafts, proposals):
    /// Triage and Writer.
    Draft,
    /// Changes mail state (archive, move, tag, flag, read): a proposal under Triage,
    /// runs directly under Writer.
    Propose,
    /// Has lasting or wide effects (rules, imports, exports, revoking tokens): asks the
    /// user every time, under Triage and Writer.
    Ask,
    /// Disabled by default (`send`). A token approved with `allow_send` may run it, and
    /// then it always asks and is never auto-approved.
    Disabled,
}

impl Permission {
    /// Every class.
    pub const ALL: [Self; 5] = [
        Self::Read,
        Self::Draft,
        Self::Propose,
        Self::Ask,
        Self::Disabled,
    ];

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Draft => "draft",
            Self::Propose => "propose",
            Self::Ask => "ask",
            Self::Disabled => "disabled",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What happens when a token runs a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    /// Runs directly.
    Allow,
    /// Becomes a proposal in the review UI.
    Propose,
    /// Waits for the user to approve it in Maze.
    Ask,
    /// Refused (`permission_denied`, exit code 3).
    Deny,
}

impl Access {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Propose => "propose",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The profile matrix: what `profile` gets for a command of class `permission`.
///
/// `allow_send` is the token's opt-in for [`Permission::Disabled`] commands.
///
/// | Permission | read-only | triage   | writer |
/// | ---------- | --------- | -------- | ------ |
/// | read       | allow     | allow    | allow  |
/// | draft      | deny      | allow    | allow  |
/// | propose    | deny      | propose  | allow  |
/// | ask        | deny      | ask      | ask    |
/// | disabled   | deny      | deny / ask with `allow_send` | deny / ask with `allow_send` |
///
/// Permanent delete is not a command at all, so no token can ever run it.
pub fn access(profile: Profile, permission: Permission, allow_send: bool) -> Access {
    use Access as A;
    use Permission as P;
    use Profile::{ReadOnly, Triage, Writer};
    match (profile, permission) {
        (_, P::Read) => A::Allow,
        (ReadOnly, _) => A::Deny,
        (Triage | Writer, P::Draft) => A::Allow,
        (Triage, P::Propose) => A::Propose,
        (Writer, P::Propose) => A::Allow,
        (Triage | Writer, P::Ask) => A::Ask,
        (Triage | Writer, P::Disabled) => {
            if allow_send {
                A::Ask
            } else {
                A::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix() {
        use Access::*;
        use Profile::*;
        #[rustfmt::skip]
        let table = [
            // permission,          read-only, triage,  writer,  triage+send, writer+send
            (Permission::Read,     [Allow,     Allow,   Allow,   Allow,       Allow]),
            (Permission::Draft,    [Deny,      Allow,   Allow,   Allow,       Allow]),
            (Permission::Propose,  [Deny,      Propose, Allow,   Propose,     Allow]),
            (Permission::Ask,      [Deny,      Ask,     Ask,     Ask,         Ask]),
            (Permission::Disabled, [Deny,      Deny,    Deny,    Ask,         Ask]),
        ];
        for (permission, expected) in table {
            let actual = [
                access(ReadOnly, permission, false),
                access(Triage, permission, false),
                access(Writer, permission, false),
                access(Triage, permission, true),
                access(Writer, permission, true),
            ];
            assert_eq!(actual, expected, "{permission}");
            // `allow_send` never helps a read-only token.
            assert_eq!(access(ReadOnly, permission, true), expected[0]);
        }
    }
}

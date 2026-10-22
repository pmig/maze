use serde::{Deserialize, Serialize};

/// The special-use role of a folder (RFC 6154), plus Inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    /// Gmail's All Mail.
    All,
    /// Gmail's Starred / `\Flagged` special-use.
    Flagged,
    /// Gmail's Important.
    Important,
}

impl FolderRole {
    /// Map a LIST attribute such as `\Sent` to a role.
    pub fn from_special_use(attr: &str) -> Option<Self> {
        Some(match attr.to_ascii_lowercase().as_str() {
            "\\sent" => Self::Sent,
            "\\drafts" => Self::Drafts,
            "\\trash" => Self::Trash,
            "\\junk" | "\\spam" => Self::Junk,
            "\\archive" => Self::Archive,
            "\\all" | "\\allmail" => Self::All,
            "\\flagged" | "\\starred" => Self::Flagged,
            "\\important" => Self::Important,
            "\\inbox" => Self::Inbox,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Archive => "archive",
            Self::All => "all",
            Self::Flagged => "flagged",
            Self::Important => "important",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "inbox" => Self::Inbox,
            "sent" => Self::Sent,
            "drafts" => Self::Drafts,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            "archive" => Self::Archive,
            "all" => Self::All,
            "flagged" => Self::Flagged,
            "important" => Self::Important,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_use_mapping() {
        assert_eq!(
            FolderRole::from_special_use("\\Sent"),
            Some(FolderRole::Sent)
        );
        assert_eq!(
            FolderRole::from_special_use("\\Spam"),
            Some(FolderRole::Junk)
        );
        assert_eq!(FolderRole::from_special_use("\\Noselect"), None);
        for role in [FolderRole::Inbox, FolderRole::All, FolderRole::Important] {
            assert_eq!(FolderRole::parse(role.as_str()), Some(role));
        }
    }
}

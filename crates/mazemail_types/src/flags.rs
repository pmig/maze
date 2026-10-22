use serde::{Deserialize, Serialize};

bitflags::bitflags! {
    /// IMAP system flags, plus Maze-local state. Stored per message location.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct Flags: u32 {
        const SEEN      = 1 << 0;
        const ANSWERED  = 1 << 1;
        const FLAGGED   = 1 << 2;
        const DELETED   = 1 << 3;
        const DRAFT     = 1 << 4;
        /// `$Forwarded` keyword, shown like a flag.
        const FORWARDED = 1 << 5;
        /// `$Junk` keyword.
        const JUNK      = 1 << 6;
        /// `$NotJunk` keyword.
        const NOT_JUNK  = 1 << 7;
    }
}

impl Flags {
    /// Map an IMAP flag or well-known keyword (`\Seen`, `$Junk`, …) to a flag.
    pub fn from_imap(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "\\seen" => Self::SEEN,
            "\\answered" => Self::ANSWERED,
            "\\flagged" => Self::FLAGGED,
            "\\deleted" => Self::DELETED,
            "\\draft" => Self::DRAFT,
            "$forwarded" => Self::FORWARDED,
            "$junk" | "junk" => Self::JUNK,
            "$notjunk" | "notjunk" | "nonjunk" => Self::NOT_JUNK,
            _ => return None,
        })
    }

    /// The IMAP names of the set flags, in a stable order.
    pub fn to_imap(self) -> Vec<&'static str> {
        let table = [
            (Self::SEEN, "\\Seen"),
            (Self::ANSWERED, "\\Answered"),
            (Self::FLAGGED, "\\Flagged"),
            (Self::DELETED, "\\Deleted"),
            (Self::DRAFT, "\\Draft"),
            (Self::FORWARDED, "$Forwarded"),
            (Self::JUNK, "$Junk"),
            (Self::NOT_JUNK, "$NotJunk"),
        ];
        table
            .into_iter()
            .filter(|(flag, _)| self.contains(*flag))
            .map(|(_, name)| name)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imap_names_round_trip() {
        let flags = Flags::SEEN | Flags::FLAGGED | Flags::JUNK;
        let names = flags.to_imap();
        let parsed = names
            .iter()
            .filter_map(|n| Flags::from_imap(n))
            .fold(Flags::empty(), |a, b| a | b);
        assert_eq!(parsed, flags);
        assert_eq!(Flags::from_imap("$label1"), None);
    }
}

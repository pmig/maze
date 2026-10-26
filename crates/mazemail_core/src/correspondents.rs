//! The message list's _Correspondents_ column (CONCEPT §8.2), as in Thunderbird.
//!
//! - **Received mail** shows the sender.
//! - **Mail you sent** shows the recipients, with a "sent" arrow in the UI.
//!
//! A message counts as sent by you ([`is_outgoing`]) when its `From` address is one of
//! your identities' addresses (Thunderbird's `nsMsgDBView::IsOutgoingMsg`: compared
//! case-insensitively, with `+tag` sub-addressing dropped), or when it has a copy in a
//! Sent, Drafts or Outbox folder (identities can be missing, for example for mail
//! uploaded from an old mbox file).

use std::collections::HashSet;

use mazemail_types::Address;

/// The addresses of your identities, normalized for [`is_outgoing`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityEmails(HashSet<String>);

impl IdentityEmails {
    /// The normalized set of `emails`.
    pub fn new<'a>(emails: impl IntoIterator<Item = &'a str>) -> Self {
        Self(emails.into_iter().filter_map(normalize_email).collect())
    }

    /// Whether `email` is one of the identities' addresses.
    pub fn contains(&self, email: &str) -> bool {
        normalize_email(email).is_some_and(|email| self.0.contains(&email))
    }

    /// Whether there are no identities.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Lowercases an address and drops `+tag` sub-addressing from the local part
/// (`Philip+lists@Example.com` → `philip@example.com`). `None` for blank input.
pub fn normalize_email(email: &str) -> Option<String> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return None;
    }
    Some(match email.split_once('@') {
        Some((local, domain)) => {
            let local = local.split_once('+').map_or(local, |(base, _)| base);
            format!("{local}@{domain}")
        }
        None => email,
    })
}

/// Whether a message is mail you sent: its sender is one of your identities, or it has
/// a copy in a Sent, Drafts or Outbox folder.
pub fn is_outgoing(
    from: Option<&Address>,
    in_sent_folder: bool,
    identities: &IdentityEmails,
) -> bool {
    in_sent_folder || from.is_some_and(|from| identities.contains(&from.email))
}

/// What the Correspondents column shows for a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Correspondents<'a> {
    /// Received mail: the sender (if the message has one).
    Sender(Option<&'a Address>),
    /// Mail you sent: the `To` recipients, or the `Cc` recipients if there are no `To`
    /// recipients.
    Recipients(&'a [Address]),
}

impl Correspondents<'_> {
    /// Whether this is mail you sent.
    pub fn is_outgoing(&self) -> bool {
        matches!(self, Self::Recipients(_))
    }

    /// The column text: display names (or addresses) joined with ", ".
    pub fn text(&self) -> String {
        match self {
            Self::Sender(from) => from.map(display).unwrap_or_default(),
            Self::Recipients(to) => to.iter().map(display).collect::<Vec<_>>().join(", "),
        }
    }
}

/// The correspondents of a message from its headers (see the [module docs](self)).
pub fn correspondents<'a>(
    from: Option<&'a Address>,
    to: &'a [Address],
    cc: &'a [Address],
    outgoing: bool,
) -> Correspondents<'a> {
    if outgoing {
        Correspondents::Recipients(if to.is_empty() { cc } else { to })
    } else {
        Correspondents::Sender(from)
    }
}

/// How the list shows a person: the display name, or the address if there's none.
pub fn display(address: &Address) -> String {
    address
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&address.email)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(name: Option<&str>, email: &str) -> Address {
        Address::new(name, email)
    }

    #[test]
    fn normalizes_case_and_sub_addresses() {
        assert_eq!(
            normalize_email(" Philip+Lists@Example.COM ").as_deref(),
            Some("philip@example.com")
        );
        assert_eq!(normalize_email("local").as_deref(), Some("local"));
        assert_eq!(normalize_email("  "), None);
    }

    #[test]
    fn received_mail_shows_the_sender() {
        let me = IdentityEmails::new(["philip@example.com"]);
        let anna = addr(Some("Anna Example"), "anna@example.com");
        let to = [addr(Some("Philip"), "philip@example.com")];
        let outgoing = is_outgoing(Some(&anna), false, &me);
        assert!(!outgoing);
        let c = correspondents(Some(&anna), &to, &[], outgoing);
        assert_eq!(c, Correspondents::Sender(Some(&anna)));
        assert_eq!(c.text(), "Anna Example");
        assert!(!c.is_outgoing());
    }

    #[test]
    fn sent_mail_shows_every_recipient() {
        let me = IdentityEmails::new(["philip@example.com", "philip@work.example"]);
        let from = addr(Some("Philip"), "Philip+maze@Work.Example");
        let to = [
            addr(Some("Anna Example"), "anna@example.com"),
            addr(None, "bob@example.com"),
            addr(Some("  "), "carol@example.com"),
        ];
        let outgoing = is_outgoing(Some(&from), false, &me);
        assert!(outgoing, "identity match ignores case and +tags");
        let c = correspondents(Some(&from), &to, &[], outgoing);
        assert!(c.is_outgoing());
        assert_eq!(c.text(), "Anna Example, bob@example.com, carol@example.com");
    }

    #[test]
    fn cc_only_and_sent_folder_copies() {
        let nobody = IdentityEmails::default();
        let from = addr(Some("Philip"), "philip@localhost");
        let cc = [addr(Some("Team"), "team@example.com")];
        // No identities (mail uploaded from an old mbox file), but the copy is in Sent.
        assert!(!is_outgoing(Some(&from), false, &nobody));
        let outgoing = is_outgoing(Some(&from), true, &nobody);
        assert!(outgoing);
        assert_eq!(
            correspondents(Some(&from), &[], &cc, outgoing).text(),
            "Team"
        );
        // Without a sender nothing is shown for received mail.
        assert_eq!(correspondents(None, &[], &[], false).text(), "");
    }
}

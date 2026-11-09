//! Aliases: more From addresses of an account, from `accounts.<label>.aliases` in the
//! settings (Outlook's and Gmail's "send as" addresses).
//!
//! Each alias is an identity of its account, so the composer's From offers it and a
//! reply to mail sent to it picks it. It sends through the account's SMTP server with
//! the account's login, takes the default identity's signature and name (unless the
//! setting names it) and the account's Sent folder. Identities made this way carry the
//! label [`ALIAS_LABEL`]; [`sync`] creates, updates and removes only those, at startup,
//! after every settings change and after an account is added. Whether the server lets
//! the account send as the address is the provider's business: Microsoft 365 refuses
//! with *SendAsDenied* until it is allowed, and the draft stays open with the reason.

use settings::Settings;
use store::{IdentityInput, Queries as _, WriteTx};

/// The label of identities that [`sync`] manages.
pub const ALIAS_LABEL: &str = "alias";

/// Brings every account's alias identities in line with `settings`. Returns whether
/// anything changed.
pub(crate) fn sync(w: &mut WriteTx<'_>, settings: &Settings) -> store::Result<bool> {
    let mut changed = false;
    for account in w.accounts()? {
        let wanted = settings.aliases(&account.name);
        let identities = w.identities(Some(account.id))?;
        let Some(default) = identities
            .iter()
            .find(|i| i.input.is_default)
            .or_else(|| identities.first())
            .cloned()
        else {
            continue;
        };
        let managed = |label: &Option<String>| label.as_deref() == Some(ALIAS_LABEL);
        // Aliases that are gone from the settings.
        for identity in &identities {
            if managed(&identity.input.label)
                && !wanted
                    .iter()
                    .any(|a| a.email.eq_ignore_ascii_case(&identity.input.email))
            {
                w.delete_identity(identity.id)?;
                changed = true;
            }
        }
        let mut ordinal = identities
            .iter()
            .map(|i| i.input.ordinal)
            .max()
            .unwrap_or(0);
        for alias in wanted {
            let input = IdentityInput {
                label: Some(ALIAS_LABEL.to_owned()),
                display_name: alias
                    .name
                    .clone()
                    .or_else(|| default.input.display_name.clone()),
                email: alias.email.clone(),
                reply_to: alias.reply_to.clone(),
                signature: default.input.signature,
                smtp: default.input.smtp,
                sent_folder: None,
                drafts_folder: None,
                archive_folder: None,
                archive_granularity: default.input.archive_granularity,
                color: None,
                match_patterns_json: None,
                is_default: false,
                ordinal: 0,
            };
            match identities
                .iter()
                .find(|i| i.input.email.eq_ignore_ascii_case(&alias.email))
            {
                // An identity of its own (the account's address, an imported one) stays
                // as it is.
                Some(existing) if !managed(&existing.input.label) => {}
                Some(existing) => {
                    let updated = IdentityInput {
                        ordinal: existing.input.ordinal,
                        ..input
                    };
                    if existing.input != updated {
                        w.update_identity(existing.id, &updated)?;
                        changed = true;
                    }
                }
                None => {
                    ordinal += 1;
                    w.create_identity(account.id, &IdentityInput { ordinal, ..input })?;
                    changed = true;
                }
            }
        }
    }
    Ok(changed)
}

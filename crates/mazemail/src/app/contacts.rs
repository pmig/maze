//! Connecting contacts from the command palette ("Contacts: Connect Contacts",
//! "Contacts: Sync Contacts"). Every Google and Microsoft account whose contacts aren't
//! connected (or lost the permission), or whose shared mailboxes
//! (`accounts.<label>.shared_contacts`) lack permission, gets its provider's consent
//! page in the browser, one after the other; the result shows as a notification. The
//! Address Book is a space of its own.

use gpui::{App, AppContext as _};
use mazemail_core::{ContactsProvider as Provider, CoreHandle};
use mazemail_types::{AccountId, PRODUCT_NAME};
use ui::kit::component::{WindowExt as _, notification::Notification};

use super::AppState;

/// Connects the contacts of every Google and Microsoft account that hasn't connected
/// them.
pub(crate) fn connect_contacts(cx: &mut App) {
    let Some(core) = cx.global::<AppState>().core.clone() else {
        notify("The mail data isn't open yet.".to_owned(), cx);
        return;
    };
    let pending = match pending_accounts(&core) {
        Ok(pending) => pending,
        Err(error) => {
            notify(format!("Contacts can't be connected: {error}"), cx);
            return;
        }
    };
    match pending {
        Pending::NoAccount => notify(
            "Contacts can be connected for accounts that sign in with Google or Microsoft. \
             Add one with Add Account first."
                .to_owned(),
            cx,
        ),
        Pending::AllConnected => {
            core.sync_contacts_now(None);
            notify(
                "The contacts of every Google and Microsoft account are connected; Maze is \
                 syncing them."
                    .to_owned(),
                cx,
            );
        }
        Pending::Accounts(accounts) => {
            notify(
                format!(
                    "Continue in your browser: sign in and allow {PRODUCT_NAME} to see and \
                     edit your contacts…"
                ),
                cx,
            );
            let task = cx.background_spawn(async move {
                let mut results = Vec::with_capacity(accounts.len());
                for mut account in accounts {
                    let result = core.connect_contacts(account.id).await;
                    // Whether the shared mailboxes were granted, as the core saw it.
                    if let Ok(status) = core.contacts_status()
                        && let Some(now) = status.iter().find(|s| s.account == account.id)
                    {
                        account.shared_granted = now.shared_permission;
                    }
                    results.push((account, result));
                }
                results
            });
            cx.spawn(async move |cx| {
                let results = task.await;
                cx.update(|cx| {
                    for (account, result) in results {
                        notify(outcome(&account, result), cx);
                    }
                });
            })
            .detach();
        }
    }
}

/// Syncs the contacts of every connected account now.
pub(crate) fn sync_contacts(cx: &mut App) {
    if let Some(core) = cx.global::<AppState>().core.as_ref() {
        core.sync_contacts_now(None);
    }
}

enum Pending {
    NoAccount,
    AllConnected,
    Accounts(Vec<PendingAccount>),
}

/// An account whose contacts (or shared mailboxes) aren't connected.
struct PendingAccount {
    id: AccountId,
    name: String,
    provider: Provider,
    /// Shared mailboxes it should sync.
    shared: usize,
    /// They are granted (after connecting).
    shared_granted: bool,
}

fn pending_accounts(core: &CoreHandle) -> mazemail_core::Result<Pending> {
    let status = core.contacts_status()?;
    if status.is_empty() {
        return Ok(Pending::NoAccount);
    }
    let names = core.accounts()?;
    let pending: Vec<PendingAccount> = status
        .iter()
        .filter(|s| {
            !s.connected
                || !s.permission
                || (!s.shared_mailboxes.is_empty() && !s.shared_permission)
        })
        .map(|s| PendingAccount {
            id: s.account,
            name: names
                .iter()
                .find(|a| a.id == s.account)
                .map(|a| a.name.clone())
                .unwrap_or_default(),
            provider: s.provider,
            shared: s.shared_mailboxes.len(),
            shared_granted: s.shared_permission,
        })
        .collect();
    Ok(if pending.is_empty() {
        Pending::AllConnected
    } else {
        Pending::Accounts(pending)
    })
}

fn outcome(account: &PendingAccount, result: mazemail_core::Result<bool>) -> String {
    let (name, provider) = (&account.name, account.provider.name());
    match result {
        Ok(true) if account.shared > 0 && !account.shared_granted => format!(
            "The {provider} contacts of “{name}” are connected, but not the shared mailboxes: \
             allow access to shared contacts (your organization may have to approve it), \
             then connect again."
        ),
        Ok(true) => format!(
            "The {provider} contacts of “{name}” are connected; {PRODUCT_NAME} is syncing them."
        ),
        Ok(false) => format!(
            "{provider} didn't give {PRODUCT_NAME} access to the contacts of “{name}”. Connect \
             again and allow access to your contacts. Mail isn't affected."
        ),
        Err(error) => format!("The contacts of “{name}” couldn't be connected: {error}"),
    }
}

/// Shows `message` as a notification in the main window.
fn notify(message: String, cx: &mut App) {
    let Some(window) = super::main_window(cx) else {
        tracing::info!("{message}");
        return;
    };
    let shown = window.update(cx, |_, window, cx| {
        window.push_notification(Notification::info(message).title(PRODUCT_NAME), cx);
    });
    if let Err(error) = shown {
        tracing::debug!(%error, "no main window for a contacts notification");
    }
}

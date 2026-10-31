//! Connecting Google contacts from the command palette ("Contacts: Connect Google
//! Contacts", "Contacts: Sync Contacts"). Every Google account whose contacts aren't
//! connected (or lost the permission) gets Google's consent page in the browser, one
//! after the other; the result shows as a notification. The Address Book is a space of
//! its own.

use gpui::{App, AppContext as _};
use mazemail_core::CoreHandle;
use mazemail_types::{AccountId, PRODUCT_NAME};
use ui::kit::component::{WindowExt as _, notification::Notification};

use super::AppState;

/// Connects the contacts of every Google account that hasn't connected them.
pub(crate) fn connect_google_contacts(cx: &mut App) {
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
        Pending::NoGoogleAccount => notify(
            "Google contacts can be connected for accounts that sign in with Google. \
             Add one with Add Account first."
                .to_owned(),
            cx,
        ),
        Pending::AllConnected => {
            core.sync_contacts_now(None);
            notify(
                "The contacts of every Google account are connected; Maze is syncing them."
                    .to_owned(),
                cx,
            );
        }
        Pending::Accounts(accounts) => {
            notify(
                format!(
                    "Continue in your browser: sign in with Google and allow {PRODUCT_NAME} \
                     to see and edit your contacts…"
                ),
                cx,
            );
            let task = cx.background_spawn(async move {
                let mut results = Vec::with_capacity(accounts.len());
                for (id, name) in accounts {
                    results.push((name, core.connect_google_contacts(id).await));
                }
                results
            });
            cx.spawn(async move |cx| {
                let results = task.await;
                cx.update(|cx| {
                    for (name, result) in results {
                        notify(outcome(&name, result), cx);
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
    NoGoogleAccount,
    AllConnected,
    Accounts(Vec<(AccountId, String)>),
}

fn pending_accounts(core: &CoreHandle) -> mazemail_core::Result<Pending> {
    let status = core.contacts_status()?;
    if status.is_empty() {
        return Ok(Pending::NoGoogleAccount);
    }
    let names = core.accounts()?;
    let pending: Vec<(AccountId, String)> = status
        .iter()
        .filter(|s| !s.connected || !s.permission)
        .map(|s| {
            let name = names
                .iter()
                .find(|a| a.id == s.account)
                .map(|a| a.name.clone())
                .unwrap_or_default();
            (s.account, name)
        })
        .collect();
    Ok(if pending.is_empty() {
        Pending::AllConnected
    } else {
        Pending::Accounts(pending)
    })
}

fn outcome(name: &str, result: mazemail_core::Result<bool>) -> String {
    match result {
        Ok(true) => format!(
            "The Google contacts of “{name}” are connected; {PRODUCT_NAME} is syncing them."
        ),
        Ok(false) => format!(
            "Google didn't give {PRODUCT_NAME} access to the contacts of “{name}”. Connect \
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

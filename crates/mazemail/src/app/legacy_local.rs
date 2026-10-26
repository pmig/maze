//! Local Folders of an older Maze (ADR-019): Maze is IMAP only now. Local accounts
//! that still hold mail stay readable and exportable until a later version removes
//! them; at every start a notification says so and how to keep the mail.

use gpui::App;
use mazemail_core::CoreHandle;
use mazemail_types::PRODUCT_NAME;
use ui::kit::component::{WindowExt as _, notification::Notification};

/// Shows the notice if any legacy local account still holds mail.
pub(crate) fn notify(core: &CoreHandle, cx: &mut App) {
    let accounts = match core.legacy_local_accounts() {
        Ok(accounts) => accounts,
        Err(error) => {
            tracing::warn!(%error, "cannot look for Local Folders of an older Maze");
            return;
        }
    };
    let names: Vec<String> = accounts.into_iter().map(|(a, _)| a.name).collect();
    let Some(message) = notice(&names) else {
        return;
    };
    tracing::info!(
        accounts = names.len(),
        "Local Folders of an older Maze still hold mail"
    );
    let Some(window) = super::main_window(cx) else {
        return;
    };
    let shown = window.update(cx, |_, window, cx| {
        window.push_notification(Notification::warning(message).title(PRODUCT_NAME), cx);
    });
    if let Err(error) = shown {
        tracing::debug!(%error, "no main window for the Local Folders notice");
    }
}

/// The notice for the legacy local accounts named `names`, `None` if there are none.
fn notice(names: &[String]) -> Option<String> {
    let first = names.first()?;
    let which = match names {
        [_] => format!("“{first}”"),
        _ => names
            .iter()
            .map(|n| format!("“{n}”"))
            .collect::<Vec<_>>()
            .join(", "),
    };
    let query = format!("account:\"{}\"", first.replace('"', ""));
    Some(format!(
        "Local Folders are no longer supported ({which}). Export them with \
         `mazemail export '{query}' --to local-folders.mbox` and upload the file into an \
         IMAP account with `mazemail import local-folders.mbox --account …`. They'll be \
         removed in a later version."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_says_how_to_keep_the_mail() {
        assert_eq!(notice(&[]), None);
        let one = notice(&["Local Folders".to_owned()]).unwrap();
        assert!(one.starts_with("Local Folders are no longer supported (“Local Folders”)."));
        assert!(
            one.contains("`mazemail export 'account:\"Local Folders\"' --to local-folders.mbox`")
        );
        assert!(one.contains("mazemail import local-folders.mbox --account"));
        assert!(one.contains("removed in a later version"));
        let two = notice(&["Local Folders".to_owned(), "Old".to_owned()]).unwrap();
        assert!(two.contains("(“Local Folders”, “Old”)"));
    }
}

//! System notifications for new mail (the `notifications` settings, ADR-021).
//!
//! The core reports newly arrived mail in Inboxes and in favorite folders other than
//! Sent ([`CoreHandle::new_mail`]); what arrives within
//! [`GATHER`] becomes notifications through the [`notifications`] crate: one per
//! message (sender, subject and, with several accounts, the account), or a single
//! "5 new messages" when more than [`MAX_SINGLE`] arrive together.
//! `notifications.content` decides how much they show, `notifications.enabled` turns
//! them off. A click brings Maze to the front and opens the message in a tab.

use std::collections::BTreeSet;
use std::time::Duration;

use gpui::App;
use mazemail_core::{CoreHandle, NewMail};
use mazemail_types::MessageId;
use notifications::{AppInfo, Notice, Notifier};
use release_channel::ReleaseChannel;
use settings::NotificationContent;
use tokio::sync::{broadcast, mpsc};

/// New mail arriving within this long after the first message is shown together.
const GATHER: Duration = Duration::from_millis(800);

/// More messages than this at once get one summary notification.
const MAX_SINGLE: usize = 3;

/// Senders a summary names before "and 2 others".
const SUMMARY_SENDERS: usize = 3;

/// What a click on a notification opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// The message, in a tab.
    Message(MessageId),
    /// Just the window.
    Window,
}

/// Shows notifications for the core's new mail until the core closes.
pub(crate) fn start(core: &CoreHandle, cx: &mut App) {
    let channel = ReleaseChannel::current();
    let notifier = Notifier::new(
        AppInfo {
            name: channel.app_name().to_owned(),
            id: channel.bundle_id().to_owned(),
            icon_dir: Some(core.data_dir().join("icons")),
        },
        core.runtime(),
    );
    let (clicks, mut clicked) = mpsc::unbounded_channel::<Target>();
    let mut arrivals = core.new_mail();
    let core = core.clone();
    cx.spawn(async move |cx| {
        loop {
            let mut mail = match arrivals.recv().await {
                Ok(batch) => batch.to_vec(),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            cx.background_executor().timer(GATHER).await;
            loop {
                match arrivals.try_recv() {
                    Ok(batch) => mail.extend(batch.iter().cloned()),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
            let settings = core.settings().settings.notifications.clone();
            if !settings.enabled {
                continue;
            }
            let several_accounts = core.accounts().is_ok_and(|accounts| accounts.len() > 1);
            for (notice, target) in notices(&mail, settings.content, several_accounts) {
                let clicks = clicks.clone();
                notifier.show(notice, move || {
                    let _ = clicks.send(target);
                });
            }
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Some(target) = clicked.recv().await {
            cx.update(|cx| open(target, cx));
        }
    })
    .detach();
}

/// Brings the main window to the front and opens what was clicked.
fn open(target: Target, cx: &mut App) {
    cx.activate(true);
    let Some(handle) = super::main_window(cx) else {
        return;
    };
    let mail = cx
        .try_global::<super::AppState>()
        .and_then(|state| state.window.as_ref())
        .map(|(_, mail)| mail.clone());
    let shown = handle.update(cx, |_, window, cx| {
        window.activate_window();
        if let (Target::Message(message), Some(mail)) = (target, mail) {
            mail.update(cx, |mail, cx| mail.open_message(message, None, window, cx));
        }
    });
    if let Err(error) = shown {
        tracing::debug!(%error, "no main window for a notification click");
    }
}

/// The notifications for `mail`, with what a click on each opens.
fn notices(
    mail: &[NewMail],
    content: NotificationContent,
    several_accounts: bool,
) -> Vec<(Notice, Target)> {
    if mail.len() > MAX_SINGLE {
        let body = match content {
            NotificationContent::SenderAndSubject | NotificationContent::SenderOnly => {
                Some(summary_senders(mail))
            }
            NotificationContent::None => None,
        };
        let notice = Notice {
            title: format!("{} new messages", mail.len()),
            body,
            ..Notice::default()
        };
        return vec![(notice, Target::Window)];
    }
    mail.iter()
        .map(|m| {
            let subtitle = several_accounts.then(|| m.account_name.clone());
            let notice = match content {
                NotificationContent::SenderAndSubject => Notice {
                    title: sender(m),
                    subtitle,
                    body: Some(
                        m.subject
                            .expose()
                            .as_deref()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .unwrap_or("(no subject)")
                            .to_owned(),
                    ),
                    group: Some(m.thread.to_string()),
                },
                NotificationContent::SenderOnly => Notice {
                    title: sender(m),
                    subtitle,
                    body: Some("New message".to_owned()),
                    group: Some(m.thread.to_string()),
                },
                NotificationContent::None => Notice {
                    title: "New message".to_owned(),
                    subtitle,
                    ..Notice::default()
                },
            };
            (notice, Target::Message(m.message))
        })
        .collect()
}

/// The sender as people know them: the display name, else the address.
fn sender(mail: &NewMail) -> String {
    mail.from
        .expose()
        .as_ref()
        .map(|from| {
            from.name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .unwrap_or(&from.email)
                .to_owned()
        })
        .unwrap_or_else(|| "Unknown sender".to_owned())
}

/// "From Bob, Carol and Dave", or "From Bob, Carol, Dave and 2 others".
fn summary_senders(mail: &[NewMail]) -> String {
    let mut seen = BTreeSet::new();
    let senders: Vec<String> = mail
        .iter()
        .map(sender)
        .filter(|name| seen.insert(name.clone()))
        .collect();
    let named = &senders[..senders.len().min(SUMMARY_SENDERS)];
    let others = senders.len() - named.len();
    let list = match (named, others) {
        ([], _) => String::new(),
        ([only], 0) => only.clone(),
        ([rest @ .., last], 0) => format!("{} and {last}", rest.join(", ")),
        (_, 1) => format!("{} and 1 other", named.join(", ")),
        (_, n) => format!("{} and {n} others", named.join(", ")),
    };
    format!("From {list}")
}

#[cfg(test)]
mod tests {
    use mazemail_types::{AccountId, Address, FolderId, PublicId as _, Sensitive, ThreadId};

    use super::*;

    fn mail(n: i64, name: &str) -> NewMail {
        NewMail {
            message: MessageId::from_raw(n),
            thread: ThreadId::from_raw(n),
            account: AccountId::from_raw(1),
            account_name: "Work".to_owned(),
            folder: FolderId::from_raw(1),
            from: Sensitive::new(Some(Address {
                name: (!name.is_empty()).then(|| name.to_owned()),
                email: format!("{}@example.net", name.to_lowercase()),
            })),
            subject: Sensitive::new(Some(format!("Report {n}"))),
            received: 0,
        }
    }

    #[test]
    fn a_few_messages_get_one_notification_each() {
        let shown = notices(
            &[mail(1, "Bob"), mail(2, "")],
            NotificationContent::SenderAndSubject,
            false,
        );
        assert_eq!(shown.len(), 2);
        assert_eq!(shown[0].0.title, "Bob");
        assert_eq!(shown[0].0.body.as_deref(), Some("Report 1"));
        assert_eq!(shown[0].0.subtitle, None);
        assert_eq!(shown[0].1, Target::Message(MessageId::from_raw(1)));
        // Without a display name, the address.
        assert_eq!(shown[1].0.title, "@example.net");
    }

    #[test]
    fn content_settings_hide_the_subject_or_everything() {
        let sender_only = notices(&[mail(1, "Bob")], NotificationContent::SenderOnly, true);
        assert_eq!(sender_only[0].0.title, "Bob");
        assert_eq!(sender_only[0].0.body.as_deref(), Some("New message"));
        // With several accounts the account is named.
        assert_eq!(sender_only[0].0.subtitle.as_deref(), Some("Work"));

        let none = notices(&[mail(1, "Bob")], NotificationContent::None, false);
        assert_eq!(none[0].0.title, "New message");
        assert_eq!(none[0].0.body, None);
    }

    #[test]
    fn many_messages_get_one_summary() {
        let many: Vec<NewMail> = ["Bob", "Carol", "Bob", "Dave"]
            .iter()
            .enumerate()
            .map(|(n, name)| mail(n as i64 + 1, name))
            .collect();
        let shown = notices(&many, NotificationContent::SenderAndSubject, false);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].0.title, "4 new messages");
        assert_eq!(shown[0].0.body.as_deref(), Some("From Bob, Carol and Dave"));
        assert_eq!(shown[0].1, Target::Window);

        let none = notices(&many, NotificationContent::None, false);
        assert_eq!(none[0].0.body, None);
    }

    #[test]
    fn summaries_count_the_senders_they_dont_name() {
        let names = ["Bob", "Carol", "Dave", "Erin", "Frank"];
        let many: Vec<NewMail> = names
            .iter()
            .enumerate()
            .map(|(n, name)| mail(n as i64 + 1, name))
            .collect();
        assert_eq!(summary_senders(&many), "From Bob, Carol, Dave and 2 others");
        assert_eq!(
            summary_senders(&many[..4]),
            "From Bob, Carol, Dave and 1 other"
        );
        assert_eq!(summary_senders(&many[..1]), "From Bob");
    }
}

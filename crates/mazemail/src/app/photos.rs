//! Contact photos for the avatars in the Address Book and the composer: the app's
//! loader for [`ui::contact_photos`], over the core. A synced contact's photo from
//! Google or Microsoft, else, when `contacts.gravatar` allows it, the address's
//! Gravatar (ADR-032). The photos are loaded again when that setting changes.

use std::time::Duration;

use gpui::App;
use mazemail_core::CoreHandle;
use ui::contact_photos::{PhotoData, refresh_contact_photos, set_photo_loader};

/// How long one photo may take (fetching one gives up after 15 seconds).
const PHOTO_TIMEOUT: Duration = Duration::from_secs(20);

/// Installs the loader and follows the Gravatar setting.
pub(crate) fn follow_core(core: &CoreHandle, cx: &mut App) {
    let loader = core.clone();
    set_photo_loader(cx, move |email| load(&loader, email));

    let mut settings = core.settings_watch();
    cx.spawn(async move |cx| {
        let mut gravatar = settings.borrow_and_update().settings.contacts.gravatar;
        while settings.changed().await.is_ok() {
            let now = settings.borrow_and_update().settings.contacts.gravatar;
            if now != gravatar {
                gravatar = now;
                cx.update(refresh_contact_photos);
            }
        }
    })
    .detach();
}

/// The photo of `email`, waiting for the core (on one of GPUI's background threads).
fn load(core: &CoreHandle, email: &str) -> Option<PhotoData> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let (runner, email) = (core.clone(), email.to_owned());
    core.runtime().spawn(async move {
        let _ = tx.send(runner.contact_photo(email, None).await);
    });
    match rx.recv_timeout(PHOTO_TIMEOUT) {
        Ok(Ok(photo)) => photo.map(|photo| PhotoData {
            content_type: photo.content_type,
            bytes: photo.bytes,
        }),
        Ok(Err(error)) => {
            tracing::debug!(%error, "cannot load a contact photo");
            None
        }
        Err(_) => None,
    }
}

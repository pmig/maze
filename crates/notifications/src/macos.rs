//! macOS: `UNUserNotificationCenter` through `mac-usernotifications`.

use mac_usernotifications::{AuthorizationStatus, Notification};

use crate::{AppInfo, Error, Notice, Response};

/// Checks for the app bundle and the user's answer so far, without asking.
pub(crate) async fn check(_app: &AppInfo) -> Result<&'static str, Error> {
    mac_usernotifications::check_bundle().map_err(|e| Error::Unavailable(e.to_string()))?;
    let settings = mac_usernotifications::get_notification_settings()
        .await
        .map_err(|e| Error::Service(e.to_string()))?;
    match settings.authorization_status {
        AuthorizationStatus::Denied => Err(Error::Unavailable(
            "not allowed in System Settings → Notifications".to_owned(),
        )),
        AuthorizationStatus::NotDetermined => Ok("macOS asks before the first one"),
        _ => Ok("allowed"),
    }
}

/// Checks for the app bundle and asks the user for permission (macOS shows its
/// prompt once and remembers the answer in System Settings → Notifications).
pub(crate) async fn prepare(_app: &AppInfo) -> Result<(), Error> {
    mac_usernotifications::check_bundle().map_err(|e| Error::Unavailable(e.to_string()))?;
    match mac_usernotifications::request_auth().await {
        Ok(true) => Ok(()),
        Ok(false) => Err(Error::Unavailable(
            "not allowed in System Settings → Notifications".to_owned(),
        )),
        Err(e) => Err(Error::Service(e.to_string())),
    }
}

pub(crate) async fn show(_app: &AppInfo, notice: Notice) -> Result<Response, Error> {
    let mut notification = Notification::new()
        .title(notice.title)
        .maybe_subtitle(notice.subtitle)
        .default_sound();
    if let Some(body) = notice.body {
        notification = notification.message(body);
    }
    if let Some(group) = notice.group {
        notification = notification.thread_id(group);
    }
    let handle = notification
        .send()
        .await
        .map_err(|e| Error::Service(e.to_string()))?;
    let response = handle
        .response()
        .await
        .map_err(|e| Error::Service(e.to_string()))?;
    Ok(if response.is_default_action() {
        Response::Clicked
    } else {
        Response::Closed
    })
}

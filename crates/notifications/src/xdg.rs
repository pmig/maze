//! Linux and the BSDs: the freedesktop notification service over D-Bus.

use std::path::{Path, PathBuf};

use notify_rust::{Hint, Notification};
use tokio::sync::OnceCell;

use crate::{AppInfo, Error, Notice, Response, combined_body, escape_markup};

/// The freedesktop category for new mail.
const CATEGORY: &str = "email.arrived";

/// Maze's logo (512 px), the notification icon.
const LOGO: &[u8] = include_bytes!("../../../assets/icons/maze.png");

/// Whether the server renders body markup, asked once.
static BODY_MARKUP: OnceCell<bool> = OnceCell::const_new();

/// The icon notifications name: the logo's file, or the icon theme name.
static ICON: OnceCell<String> = OnceCell::const_new();

/// Nothing to ask for; a missing service shows up as an error per notice.
pub(crate) async fn prepare(_app: &AppInfo) -> Result<(), Error> {
    Ok(())
}

pub(crate) async fn show(app: &AppInfo, notice: Notice) -> Result<Response, Error> {
    let markup = *BODY_MARKUP.get_or_init(body_markup).await;
    let icon = ICON.get_or_init(|| icon(app)).await;
    let mut notification = Notification::new();
    notification
        .appname(&app.name)
        .summary(&notice.title)
        .icon(icon)
        .hint(Hint::DesktopEntry(app.id.clone()))
        .hint(Hint::Category(CATEGORY.to_owned()))
        // The "default" action is a click on the notification itself.
        .action("default", "Open");
    if let Some(body) = combined_body(&notice) {
        let body = if markup { escape_markup(&body) } else { body };
        notification.body(&body);
    }
    let handle = notification
        .show_async()
        .await
        .map_err(|e| Error::Service(e.to_string()))?;
    let mut clicked = false;
    handle
        .wait_for_action_async(|response| {
            tracing::debug!(?response, "notification response");
            clicked = response.is_default_action();
        })
        .await;
    Ok(if clicked {
        Response::Clicked
    } else {
        Response::Closed
    })
}

/// Asks the server for its capabilities (a blocking D-Bus call, off the runtime's
/// workers).
async fn body_markup() -> bool {
    let asked = tokio::task::spawn_blocking(notify_rust::get_capabilities).await;
    match asked {
        Ok(Ok(capabilities)) => capabilities.iter().any(|c| c == "body-markup"),
        Ok(Err(error)) => {
            tracing::debug!(%error, "cannot ask the notification server for its capabilities");
            // Escaping is the safe side: a server that renders markup would read raw
            // `<` as a tag.
            true
        }
        Err(error) => {
            tracing::debug!(%error, "asking for notification capabilities failed");
            true
        }
    }
}

/// The logo's file in [`AppInfo::icon_dir`] (written if it is missing or outdated), or
/// the icon theme name when there is no directory or writing fails.
async fn icon(app: &AppInfo) -> String {
    let Some(dir) = app.icon_dir.clone() else {
        return app.id.clone();
    };
    let file = dir.join(format!("{}.png", app.id));
    let written = tokio::task::spawn_blocking({
        let file = file.clone();
        move || write_logo(&dir, &file)
    })
    .await;
    match written {
        Ok(Ok(())) => file.display().to_string(),
        Ok(Err(error)) => {
            tracing::debug!(%error, "cannot write the notification icon");
            app.id.clone()
        }
        Err(error) => {
            tracing::debug!(%error, "writing the notification icon failed");
            app.id.clone()
        }
    }
}

/// Writes [`LOGO`] to `file` unless it is there already; a temporary file and a
/// rename, so the server never reads half an image.
fn write_logo(dir: &Path, file: &PathBuf) -> std::io::Result<()> {
    if std::fs::read(file).is_ok_and(|current| current == LOGO) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let partial = file.with_extension("png.partial");
    std::fs::write(&partial, LOGO)?;
    std::fs::rename(&partial, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logo_is_a_png() {
        assert!(LOGO.starts_with(b"\x89PNG\r\n\x1a\n"));
    }

    #[tokio::test]
    async fn the_logo_is_written_once_into_the_icon_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppInfo {
            name: "Maze Dev".into(),
            id: "dev.mazemail.maze-dev".into(),
            icon_dir: Some(tmp.path().join("icons")),
        };
        let path = icon(&app).await;
        let file = tmp.path().join("icons/dev.mazemail.maze-dev.png");
        assert_eq!(path, file.display().to_string());
        assert_eq!(std::fs::read(&file).unwrap(), LOGO);
        // An outdated copy is replaced.
        std::fs::write(&file, b"old").unwrap();
        icon(&app).await;
        assert_eq!(std::fs::read(&file).unwrap(), LOGO);
    }

    #[tokio::test]
    async fn without_a_directory_the_theme_icon_is_named() {
        let app = AppInfo {
            name: "Maze".into(),
            id: "dev.mazemail.maze".into(),
            icon_dir: None,
        };
        assert_eq!(icon(&app).await, "dev.mazemail.maze");
    }
}

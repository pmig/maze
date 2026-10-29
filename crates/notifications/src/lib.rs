//! System notifications for new mail (ADR-021).
//!
//! [`Notifier::show`] hands a [`Notice`] to the platform's notification service and
//! calls back when the user clicks it:
//!
//! - **macOS:** `UNUserNotificationCenter` through `mac-usernotifications`. It needs
//!   the signed `Maze.app` bundle (a plain `cargo run` has no bundle identifier, so
//!   notifications are off there) and the user's permission, which macOS asks for
//!   before the first notification. Banners show while Maze is in front, too.
//! - **Linux and the BSDs:** the freedesktop notification service over D-Bus
//!   (`notify-rust` on zbus), with Maze's desktop entry and the `email.arrived`
//!   category. The icon is Maze's logo, embedded and written once to
//!   [`AppInfo::icon_dir`], so it shows without an installed icon theme entry (dev
//!   builds, unpacked tarballs). The body is escaped when the server renders markup.
//!   On macOS Notification Center always shows the icon of `Maze.app`.
//! - Elsewhere notifications are unavailable.
//!
//! Every notice waits for its response on the given Tokio runtime; at most
//! [`MAX_WAITING`] wait at once, and clicks on older ones are ignored. Unavailable
//! notifications are logged once and then skipped quietly. Notice text is mail
//! content and never logged.
//!
//! This crate is layer L1 and has no GPUI dependency.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::runtime::Handle;
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(all(unix, not(target_os = "macos")))]
#[path = "xdg.rs"]
mod platform;
#[cfg(not(unix))]
#[path = "unsupported.rs"]
mod platform;

/// Notices whose response is still awaited at most; the oldest stops waiting first.
pub const MAX_WAITING: usize = 32;

/// Why a notification couldn't be shown.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Notifications can't be shown here (no app bundle, not allowed, no service).
    #[error("notifications are unavailable: {0}")]
    Unavailable(String),
    /// The notification service failed.
    #[error("the notification service failed: {0}")]
    Service(String),
}

/// The app that sends the notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    /// The name people see: "Maze".
    pub name: String,
    /// The bundle ID, which is also the desktop entry and icon name on Linux:
    /// `dev.mazemail.maze`.
    pub id: String,
    /// Where the Linux backend may keep a copy of Maze's logo for the notification
    /// server (`<data dir>/icons`). Without it, the icon is looked up by [`id`](Self::id)
    /// in the icon theme.
    pub icon_dir: Option<PathBuf>,
}

/// One notification.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Notice {
    /// The first line: the sender, or "3 new messages".
    pub title: String,
    /// A second, smaller line (the account), if any.
    pub subtitle: Option<String>,
    /// The text: the subject, if any.
    pub body: Option<String>,
    /// Notices with the same group are stacked together (macOS: the thread).
    pub group: Option<String>,
}

impl fmt::Debug for Notice {
    // Mail content stays out of logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notice").finish_non_exhaustive()
    }
}

/// What the user did with a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Response {
    /// Clicked it.
    Clicked,
    /// Closed it, or it expired.
    Closed,
}

/// Shows notices; cheap to clone. See the [crate docs](crate).
#[derive(Clone)]
pub struct Notifier {
    inner: Arc<Inner>,
}

struct Inner {
    app: AppInfo,
    runtime: Handle,
    /// Whether notifications can be shown, found out before the first one.
    ready: OnceCell<bool>,
    waiting: Mutex<VecDeque<JoinHandle<()>>>,
}

impl fmt::Debug for Notifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notifier")
            .field("app", &self.inner.app)
            .field("ready", &self.inner.ready.get())
            .finish_non_exhaustive()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Notifier {
    /// A notifier for `app` that talks to the notification service on `runtime`.
    pub fn new(app: AppInfo, runtime: Handle) -> Self {
        Self {
            inner: Arc::new(Inner {
                app,
                runtime,
                ready: OnceCell::new(),
                waiting: Mutex::new(VecDeque::new()),
            }),
        }
    }

    /// Shows `notice` and calls `on_click` (on the runtime) if the user clicks it.
    /// Before the first notice, macOS asks the user for permission.
    pub fn show(&self, notice: Notice, on_click: impl FnOnce() + Send + 'static) {
        let inner = Arc::clone(&self.inner);
        let task = self.inner.runtime.spawn(async move {
            if !inner.ready().await {
                return;
            }
            match platform::show(&inner.app, notice).await {
                Ok(Response::Clicked) => on_click(),
                Ok(Response::Closed) => {}
                Err(error) => tracing::warn!(%error, "cannot show a notification"),
            }
        });
        let mut waiting = lock(&self.inner.waiting);
        waiting.retain(|task| !task.is_finished());
        waiting.push_back(task);
        while waiting.len() > MAX_WAITING {
            if let Some(oldest) = waiting.pop_front() {
                oldest.abort();
            }
        }
    }
}

impl Inner {
    /// Whether notifications can be shown; finds out (and asks) once.
    async fn ready(&self) -> bool {
        *self
            .ready
            .get_or_init(|| async {
                match platform::prepare(&self.app).await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::info!(%error, "notifications are off");
                        false
                    }
                }
            })
            .await
    }
}

/// The body and subtitle in one text, for services without a subtitle line.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub(crate) fn combined_body(notice: &Notice) -> Option<String> {
    match (&notice.subtitle, &notice.body) {
        (Some(subtitle), Some(body)) => Some(format!("{body}\n{subtitle}")),
        (Some(text), None) | (None, Some(text)) => Some(text.clone()),
        (None, None) => None,
    }
}

/// `text` with the characters freedesktop body markup gives meaning escaped.
#[cfg_attr(not(all(unix, not(target_os = "macos"))), allow(dead_code))]
pub(crate) fn escape_markup(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitle_goes_under_the_body() {
        let notice = |subtitle: Option<&str>, body: Option<&str>| Notice {
            title: "Bob".into(),
            subtitle: subtitle.map(Into::into),
            body: body.map(Into::into),
            group: None,
        };
        assert_eq!(
            combined_body(&notice(Some("Work"), Some("Report"))).as_deref(),
            Some("Report\nWork")
        );
        assert_eq!(
            combined_body(&notice(Some("Work"), None)).as_deref(),
            Some("Work")
        );
        assert_eq!(combined_body(&notice(None, None)), None);
    }

    #[test]
    fn markup_is_escaped() {
        assert_eq!(
            escape_markup("<b>Q&A</b> > 3"),
            "&lt;b&gt;Q&amp;A&lt;/b&gt; &gt; 3"
        );
    }

    #[test]
    fn notice_text_stays_out_of_debug_output() {
        let notice = Notice {
            title: "secret sender".into(),
            body: Some("secret subject".into()),
            ..Notice::default()
        };
        assert!(!format!("{notice:?}").contains("secret"));
    }
}

//! `mailto:` links from other apps (Maze as the system's mail handler, ADR-027).
//!
//! The system hands a link to `mazemail <url>` (Linux, Windows) or to the running app
//! (macOS's open-URL event). `mazemail <url>` passes it to a running Maze over the local
//! API (`mazemail open`, see [`opener`]) and otherwise starts Maze with it. Either way the
//! link becomes a new message with its recipients, subject and body filled in, which the
//! user reviews and sends; links that arrive before the mail is open wait for it.

use std::sync::Arc;

use gpui::App;
use local_api::UrlOpener;
use mail_ui::ComposeRequest;
use mail_ui::links::MailTo;
use url::Url;

use super::{AppState, compose};

/// The fields of `url` if it is a `mailto:` link.
pub(crate) fn parse(url: &str) -> Option<MailTo> {
    let url = Url::parse(url.trim()).ok()?;
    (url.scheme() == "mailto").then(|| MailTo::from_url(&url))
}

/// Opens a new message for `url`, now or once the mail is open. Returns whether it
/// was a `mailto:` link.
pub(crate) fn open(url: &str, cx: &mut App) -> bool {
    let Some(mailto) = parse(url) else {
        tracing::warn!("a link handed to Maze isn't a mailto: link; ignored");
        return false;
    };
    let ready = cx
        .try_global::<AppState>()
        .is_some_and(|s| s.core.is_some());
    if ready {
        compose::respond(ComposeRequest::MailTo(mailto), cx);
        cx.activate(true);
    } else if cx.has_global::<AppState>() {
        cx.global_mut::<AppState>().pending_links.push(mailto);
    }
    true
}

/// Opens the links that arrived while the mail was opening.
pub(crate) fn open_pending(cx: &mut App) {
    if !cx.has_global::<AppState>() {
        return;
    }
    let pending = std::mem::take(&mut cx.global_mut::<AppState>().pending_links);
    for mailto in pending {
        compose::respond(ComposeRequest::MailTo(mailto), cx);
    }
}

/// Opens every link that arrives on `links`, on GPUI's thread.
pub(crate) fn listen(links: async_channel::Receiver<String>, cx: &mut App) {
    cx.spawn(async move |cx| {
        while let Ok(url) = links.recv().await {
            cx.update(|cx| open(&url, cx));
        }
    })
    .detach();
}

/// The local API's [`UrlOpener`]: hands `mazemail open` links to [`listen`].
pub(crate) fn opener(links: async_channel::Sender<String>) -> Arc<dyn UrlOpener> {
    Arc::new(ChannelOpener { links })
}

struct ChannelOpener {
    links: async_channel::Sender<String>,
}

impl UrlOpener for ChannelOpener {
    fn open(&self, url: &str) -> bool {
        parse(url).is_some() && self.links.try_send(url.to_owned()).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_mailto_links_are_parsed() {
        let mailto = parse("mailto:anna@example.com?subject=Offsite&cc=bob@example.net").unwrap();
        assert_eq!(mailto.to, ["anna@example.com"]);
        assert_eq!(mailto.cc, ["bob@example.net"]);
        assert_eq!(mailto.subject.as_deref(), Some("Offsite"));
        assert!(parse("https://example.com").is_none());
        assert!(parse("not a url").is_none());
    }
}

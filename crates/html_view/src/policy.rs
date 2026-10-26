//! What the engine may load and where a click may go (PLAN §4.8, ADR-020).
//!
//! - **Loads:** only URLs on [`MESSAGE_ORIGIN`] are answered, from memory: the
//!   document, `cid:` parts and (if remote content is allowed) remote resources that
//!   Maze fetches itself. Everything else is refused, so the engine never touches
//!   the network or the disk.
//! - **Navigation:** the document itself may load. A click on an `http(s)` or `mailto`
//!   link opens it in the system browser (or mail handler) instead, after the link
//!   checks of the message view; anything else is denied.

use url::Url;

use crate::document::{DOCUMENT_FILE, MESSAGE_ORIGIN};

/// A resource the engine asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    /// The message document.
    Document,
    /// The inline part with this Content-ID.
    Cid(String),
    /// Remote resource number `n` of the document.
    Remote(usize),
}

/// A resource of document number `generation` (see [`crate::document::base_url`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    /// The document it belongs to.
    pub generation: u64,
    /// What it is.
    pub resource: Resource,
}

/// The resource a load is for, or `None` if it must be refused. The caller also
/// refuses resources of a document other than the one shown.
pub fn resource_for(url: &Url) -> Option<Located> {
    if !is_message_origin(url) || url.query().is_some() {
        return None;
    }
    let rest = url.path().strip_prefix("/m")?;
    let (generation, path) = rest.split_once('/')?;
    if generation.is_empty() || !generation.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let generation = generation.parse().ok()?;
    let resource = if path == DOCUMENT_FILE {
        Resource::Document
    } else if let Some(id) = path.strip_prefix("cid/") {
        Resource::Cid(percent_decode(id).filter(|id| !id.is_empty())?)
    } else {
        Resource::Remote(path.strip_prefix("remote/")?.parse().ok()?)
    };
    Some(Located {
        generation,
        resource,
    })
}

/// What to do with a navigation the page asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Navigation {
    /// The message document: load it.
    Load,
    /// A link: open it outside Maze (after the link checks).
    OpenExternally(Url),
    /// Anything else: deny.
    Deny,
}

/// The decision for a navigation to `url` while document number `generation` is the
/// one to show.
pub fn navigation_for(url: &Url, generation: u64) -> Navigation {
    if is_message_origin(url) {
        // Fragment links (`#top`) stay on the page.
        let mut without_fragment = url.clone();
        without_fragment.set_fragment(None);
        let current = Located {
            generation,
            resource: Resource::Document,
        };
        return if resource_for(&without_fragment) == Some(current) {
            Navigation::Load
        } else {
            Navigation::Deny
        };
    }
    match url.scheme() {
        "http" | "https" if url.host().is_some() => Navigation::OpenExternally(url.clone()),
        "mailto" => Navigation::OpenExternally(url.clone()),
        _ => Navigation::Deny,
    }
}

fn is_message_origin(url: &Url) -> bool {
    url.origin().ascii_serialization() == MESSAGE_ORIGIN
}

/// Decodes `%XX` escapes; `None` for invalid escapes or non-UTF-8 results.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = input.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn at(generation: u64, resource: Resource) -> Option<Located> {
        Some(Located {
            generation,
            resource,
        })
    }

    #[test]
    fn serves_only_the_message_origin() {
        assert_eq!(
            resource_for(&url("https://message.mazemail.localhost/m7/message.html")),
            at(7, Resource::Document)
        );
        assert_eq!(
            resource_for(&url("https://message.mazemail.localhost/m7/cid/logo%40x")),
            at(7, Resource::Cid("logo@x".into()))
        );
        assert_eq!(
            resource_for(&url("https://message.mazemail.localhost/m12/remote/3")),
            at(12, Resource::Remote(3))
        );
        for refused in [
            "https://message.mazemail.localhost/",
            "https://message.mazemail.localhost/message.html",
            "https://message.mazemail.localhost/m/message.html",
            "https://message.mazemail.localhost/m-1/message.html",
            "https://message.mazemail.localhost/mx/message.html",
            "https://message.mazemail.localhost/m1/remote/x",
            "https://message.mazemail.localhost/m1/cid/",
            "https://message.mazemail.localhost/m1/cid/%zz",
            "https://message.mazemail.localhost/m1/other.html",
            "https://message.mazemail.localhost/m1/message.html?x=1",
            "http://message.mazemail.localhost/m1/message.html",
            "https://message.mazemail.localhost:8443/m1/message.html",
            "https://evil.example/m1/message.html",
            "https://message.mazemail.localhost.evil.example/m1/message.html",
            "file:///etc/passwd",
            "data:text/html,hi",
        ] {
            assert_eq!(resource_for(&url(refused)), None, "{refused}");
        }
    }

    #[test]
    fn links_open_outside_and_everything_else_is_denied() {
        assert_eq!(
            navigation_for(
                &url("https://message.mazemail.localhost/m3/message.html"),
                3
            ),
            Navigation::Load
        );
        assert_eq!(
            navigation_for(
                &url("https://message.mazemail.localhost/m3/message.html#top"),
                3
            ),
            Navigation::Load
        );
        // A message the view has moved on from.
        assert_eq!(
            navigation_for(
                &url("https://message.mazemail.localhost/m2/message.html"),
                3
            ),
            Navigation::Deny
        );
        assert_eq!(
            navigation_for(&url("https://shop.example/deal?id=1"), 3),
            Navigation::OpenExternally(url("https://shop.example/deal?id=1"))
        );
        assert_eq!(
            navigation_for(&url("mailto:someone@example.com"), 3),
            Navigation::OpenExternally(url("mailto:someone@example.com"))
        );
        for denied in [
            "https://message.mazemail.localhost/m3/cid/x",
            "javascript:alert(1)",
            "file:///Users/me/.ssh/id_ed25519",
            "data:text/html,<script>x</script>",
            "about:blank",
            "ftp://files.example/x",
            "blob:https://message.mazemail.localhost/1",
            "x-apple-helpviewer://x",
        ] {
            assert_eq!(
                navigation_for(&url(denied), 3),
                Navigation::Deny,
                "{denied}"
            );
        }
    }
}

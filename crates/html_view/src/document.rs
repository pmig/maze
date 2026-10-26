//! The document the engine gets: the sanitized message HTML (PLAN §4.8) with its
//! resources pointed at [`MESSAGE_ORIGIN`], wrapped into a page with the CSP.
//!
//! The HTML comes out of `mazemail_mime`'s sanitizer (scripts, frames, objects, forms,
//! `meta refresh`, `base`, `on*` attributes and `javascript:` URLs are gone); this
//! module only rewrites resources and adds the page around it. Nothing here touches
//! the disk: the document is kept in memory and served by the load handler.

use std::fmt;

use mazemail_mime::html::{ResourceOrigin, content_security_policy, rewrite_for_embedding};
use url::Url;

/// The origin the message is served from. A host under `.localhost` never resolves to
/// the network, and every load to it is answered by Maze from memory.
pub const MESSAGE_ORIGIN: &str = "https://message.mazemail.localhost";

/// The file name of the message document.
pub const DOCUMENT_FILE: &str = "message.html";

/// The base URL of document number `generation` (`<origin>/m<generation>`). Each
/// document a view shows gets the next number, so loads of a message the view has
/// moved on from can be told apart and refused.
pub fn base_url(generation: u64) -> String {
    format!("{MESSAGE_ORIGIN}/m{generation}")
}

/// The URL of document number `generation`.
pub fn document_url(generation: u64) -> Url {
    // Built from constants and a number: always parses.
    #[allow(clippy::unwrap_used)]
    Url::parse(&format!("{}/{DOCUMENT_FILE}", base_url(generation))).unwrap()
}

fn origin(generation: u64) -> ResourceOrigin {
    // Built from constants and a number: always validates.
    #[allow(clippy::unwrap_used)]
    ResourceOrigin::new(base_url(generation)).unwrap()
}

/// Colors for a message that sets none of its own (so a plain mail matches the app
/// theme, while a designed newsletter keeps its own colors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageColors {
    /// Whether the theme is dark (sets `color-scheme`).
    pub dark: bool,
    /// Page background, a CSS color such as `#1e1e1e`.
    pub background: String,
    /// Text color.
    pub foreground: String,
    /// Link color.
    pub link: String,
}

/// How to build the document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocumentOptions {
    /// Whether remote resources may be served (the message's remote content policy).
    /// When not, `/remote/<n>` requests are refused; the engine never fetches anything
    /// itself either way.
    pub remote_allowed: bool,
    /// The app theme's colors, applied only when the message sets no colors.
    pub colors: Option<PageColors>,
}

/// A message, ready for the engine.
#[derive(Clone, PartialEq, Eq)]
pub struct OriginalDocument {
    generation: u64,
    html: String,
    csp: String,
    remote: Vec<String>,
    remote_allowed: bool,
}

impl fmt::Debug for OriginalDocument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OriginalDocument")
            .field(
                "html",
                &format_args!("<{} bytes redacted>", self.html.len()),
            )
            .field("remote", &self.remote.len())
            .field("remote_allowed", &self.remote_allowed)
            .finish()
    }
}

impl OriginalDocument {
    /// Builds document number `generation` from sanitized message HTML
    /// (`SanitizedHtml::html`).
    pub fn build(sanitized_html: &str, options: &DocumentOptions, generation: u64) -> Self {
        let origin = origin(generation);
        let rewritten = rewrite_for_embedding(sanitized_html, &origin);
        let policy = content_security_policy(&origin);
        let theme = options
            .colors
            .as_ref()
            .filter(|_| !sets_own_colors(&rewritten.html))
            .map(theme_style)
            .unwrap_or_default();
        // `<base target>` sends every link click to this frame (the embedder intercepts
        // it); it has no `href`, so `base-uri 'none'` doesn't apply.
        let html = format!(
            "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
             <meta http-equiv=\"Content-Security-Policy\" content=\"{policy}\">\
             <meta name=\"referrer\" content=\"no-referrer\">\
             <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
             <base target=\"_self\">{theme}</head><body>\n{}\n</body></html>\n",
            rewritten.html
        );
        Self {
            generation,
            html,
            // As a response header the CSP may also say `sandbox`: the page then runs
            // in a sandbox without scripts (no `allow-scripts`), forms, popups or
            // plugins, and has an opaque origin.
            csp: format!("sandbox; {policy}"),
            remote: rewritten.remote_resources,
            remote_allowed: options.remote_allowed,
        }
    }

    /// A document with exactly `html` and the CSP header `csp` (no `<meta>` CSP), to
    /// check each protection on its own.
    #[cfg(all(test, feature = "servo"))]
    pub(crate) fn with_header_only(html: &str, csp: &str) -> Self {
        Self {
            generation: 0,
            html: html.to_owned(),
            csp: csp.to_owned(),
            remote: Vec::new(),
            remote_allowed: false,
        }
    }

    /// Its number (see [`base_url`]).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Its URL.
    pub fn url(&self) -> Url {
        document_url(self.generation)
    }

    /// The complete HTML document.
    pub fn html(&self) -> &str {
        &self.html
    }

    /// The `Content-Security-Policy` response header of the document.
    pub fn content_security_policy(&self) -> &str {
        &self.csp
    }

    /// The original URL behind `/remote/<n>`, if remote content is allowed.
    pub fn remote_url(&self, n: usize) -> Option<&str> {
        if !self.remote_allowed {
            return None;
        }
        self.remote.get(n).map(String::as_str)
    }

    /// How many remote resources the message references.
    pub fn remote_count(&self) -> usize {
        self.remote.len()
    }
}

/// How many distinct remote resources sanitized message HTML references (the
/// remote content banner's count).
pub fn remote_count(sanitized_html: &str) -> usize {
    rewrite_for_embedding(sanitized_html, &origin(0))
        .remote_resources
        .len()
}

/// Whether the message HTML sets colors of its own (text, background or a background
/// image), in which case it is shown as designed.
pub fn sets_own_colors(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    [
        "color:",
        "color :",
        "bgcolor",
        "background",
        " color=",
        " text=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn theme_style(colors: &PageColors) -> String {
    let scheme = if colors.dark { "dark" } else { "light" };
    // The values come from the app theme, not from mail; keep them to color syntax.
    let clean = |value: &str| -> String {
        value
            .chars()
            .filter(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '#' | '(' | ')' | ',' | '.' | '%' | ' ')
            })
            .collect()
    };
    format!(
        "<style>:root{{color-scheme:{scheme}}}html,body{{background:{};color:{}}}a{{color:{}}}</style>",
        clean(&colors.background),
        clean(&colors.foreground),
        clean(&colors.link),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dark() -> PageColors {
        PageColors {
            dark: true,
            background: "#1e1e1e".into(),
            foreground: "#dddddd".into(),
            link: "#61afef".into(),
        }
    }

    #[test]
    fn wraps_sanitized_html_with_the_csp() {
        let doc = OriginalDocument::build("<p>Hello</p>", &DocumentOptions::default(), 1);
        assert!(doc.html().starts_with("<!DOCTYPE html>"));
        assert!(doc.html().contains("<p>Hello</p>"));
        assert!(doc.html().contains(
            "content=\"default-src 'none'; img-src https://message.mazemail.localhost/m1/ data:; \
             style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\""
        ));
        assert!(
            doc.html()
                .contains("<meta name=\"referrer\" content=\"no-referrer\">")
        );
        assert_eq!(
            doc.content_security_policy(),
            "sandbox; default-src 'none'; img-src https://message.mazemail.localhost/m1/ data:; \
             style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'"
        );
    }

    #[test]
    fn points_resources_at_the_message_origin() {
        let html = r#"<img src="cid:logo@x"><img src="https://tracker.example/a.png">
            <p style="background:url(https://e.example/bg.png)">x</p>
            <a href="https://shop.example/" target="_blank">Shop</a>"#;
        let doc = OriginalDocument::build(html, &DocumentOptions::default(), 1);
        assert!(
            doc.html()
                .contains("src=\"https://message.mazemail.localhost/m1/cid/logo@x\"")
        );
        assert!(
            doc.html()
                .contains("src=\"https://message.mazemail.localhost/m1/remote/0\"")
        );
        assert!(
            doc.html()
                .contains("message.mazemail.localhost/m1/remote/1")
        );
        assert!(!doc.html().contains("tracker.example"));
        assert!(!doc.html().contains("target=\"_blank\""));
        // Links stay links: a click goes to the navigation policy.
        assert!(doc.html().contains("href=\"https://shop.example/\""));
        assert_eq!(doc.remote_count(), 2);
    }

    #[test]
    fn remote_resources_are_served_only_when_allowed() {
        let html = r#"<img src="https://e.example/a.png">"#;
        let blocked = OriginalDocument::build(html, &DocumentOptions::default(), 1);
        assert_eq!(blocked.remote_url(0), None);
        let allowed = OriginalDocument::build(
            html,
            &DocumentOptions {
                remote_allowed: true,
                colors: None,
            },
            1,
        );
        assert_eq!(allowed.remote_url(0), Some("https://e.example/a.png"));
        assert_eq!(allowed.remote_url(1), None);
    }

    #[test]
    fn theme_colors_only_for_mail_without_colors() {
        let options = DocumentOptions {
            remote_allowed: false,
            colors: Some(dark()),
        };
        let plain = OriginalDocument::build("<p>Hi</p>", &options, 1);
        assert!(plain.html().contains("color-scheme:dark"));
        assert!(plain.html().contains("background:#1e1e1e"));
        let designed = OriginalDocument::build(
            r##"<table bgcolor="#ffffff"><tr><td style="color:#333">Hi</td></tr></table>"##,
            &options,
            1,
        );
        assert!(!designed.html().contains("color-scheme"));
        let none = OriginalDocument::build("<p>Hi</p>", &DocumentOptions::default(), 1);
        assert!(!none.html().contains("color-scheme"));
    }

    #[test]
    fn theme_values_cannot_break_out_of_the_style() {
        let options = DocumentOptions {
            remote_allowed: false,
            colors: Some(PageColors {
                background: "red}</style><script>x</script>".into(),
                ..dark()
            }),
        };
        let doc = OriginalDocument::build("<p>Hi</p>", &options, 1);
        assert!(!doc.html().contains("<script>"));
    }

    #[test]
    fn debug_output_is_redacted() {
        let doc = OriginalDocument::build("<p>secret</p>", &DocumentOptions::default(), 1);
        assert!(!format!("{doc:?}").contains("secret"));
    }
}

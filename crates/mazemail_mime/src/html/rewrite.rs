//! Resource rewriting for one output (browser tab, embedded engine).

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

use super::css::rewrite_css;
use super::dom::{Dom, NodeId};
use super::urls::{Resource, classify_resource, encode_path_segment};

/// The base URL that serves a message's resources to one output (PLAN §4.8).
///
/// For example the one-time loopback origin `http://127.0.0.1:<port>/<token>` for
/// "Open in browser", or `https://message.mazemail.localhost` for an embedded engine.
/// Always `http(s)`, without query, fragment or trailing slash, and without characters
/// that could break out of a CSP directive or an HTML attribute.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ResourceOrigin(String);

/// The string passed to [`ResourceOrigin::new`] is not a usable resource origin.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid resource origin: {reason}")]
pub struct InvalidResourceOrigin {
    reason: &'static str,
}

impl ResourceOrigin {
    /// Validate and normalize a resource origin.
    pub fn new(origin: impl AsRef<str>) -> Result<Self, InvalidResourceOrigin> {
        let raw = origin.as_ref().trim();
        let invalid = |reason| Err(InvalidResourceOrigin { reason });
        if raw.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, ';' | ',' | '\'' | '"' | '<' | '>')
        }) {
            return invalid("contains characters that are not allowed");
        }
        let Ok(url) = Url::parse(raw) else {
            return invalid("not an absolute URL");
        };
        if !matches!(url.scheme(), "http" | "https") {
            return invalid("scheme must be http or https");
        }
        if url.host().is_none() {
            return invalid("missing host");
        }
        if url.query().is_some() || url.fragment().is_some() {
            return invalid("must not have a query or fragment");
        }
        if !url.username().is_empty() || url.password().is_some() {
            return invalid("must not contain credentials");
        }
        let normalized = url.as_str().trim_end_matches('/').to_owned();
        Ok(Self(normalized))
    }

    /// The origin as a string, without trailing slash.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the origin has a path below the host (`http://127.0.0.1:1/<token>`).
    fn has_path(&self) -> bool {
        Url::parse(&self.0).is_ok_and(|u| u.path() != "/" && !u.path().is_empty())
    }

    /// URL that serves the inline part with this Content-ID.
    pub fn cid_url(&self, content_id: &str) -> String {
        format!("{}/cid/{}", self.0, encode_path_segment(content_id))
    }

    /// URL that serves remote resource number `n` of a [`RewrittenHtml`].
    pub fn remote_url(&self, n: usize) -> String {
        format!("{}/remote/{n}", self.0)
    }
}

impl fmt::Display for ResourceOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ResourceOrigin {
    type Error = InvalidResourceOrigin;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ResourceOrigin> for String {
    fn from(origin: ResourceOrigin) -> Self {
        origin.0
    }
}

/// Sanitized HTML with its resources pointed at one [`ResourceOrigin`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewrittenHtml {
    /// The HTML fragment with `cid:` and remote URLs rewritten.
    pub html: String,
    /// Remote resource table: `<origin>/remote/<n>` stands for `remote_resources[n]`.
    /// Each URL appears once. Maze fetches these itself, and only when remote content
    /// is allowed for the message.
    pub remote_resources: Vec<String>,
}

impl fmt::Debug for RewrittenHtml {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RewrittenHtml")
            .field(
                "html",
                &format_args!("<{} bytes redacted>", self.html.len()),
            )
            .field("remote_resources", &self.remote_resources.len())
            .finish()
    }
}

/// Point every resource of sanitized HTML at `origin`.
///
/// `cid:<id>` becomes `<origin>/cid/<id>`, remote `http(s)` URLs become
/// `<origin>/remote/<n>` with `n` indexing [`RewrittenHtml::remote_resources`], and
/// `data:` images stay inline. Covers `img src`, `background` attributes, inline
/// `style` and `<style>` elements. Links (`a href`) are not resources and stay as is.
///
/// The input must come from [`sanitize`](super::sanitize); this pass doesn't
/// sanitize again.
pub fn rewrite_resources(sanitized_html: &str, origin: &ResourceOrigin) -> RewrittenHtml {
    let mut dom = Dom::parse_fragment(sanitized_html);
    let root = dom.root();
    let mut table = RemoteTable::default();
    for node in dom.descendants(root) {
        match dom.tag(node) {
            Some("img") => rewrite_attr(&mut dom, node, "src", origin, &mut table),
            Some("style") => {
                for child in dom.children(node).to_vec() {
                    if let Some(css) = dom.text(child) {
                        let rewritten = rewrite_css(css, |url| map_url(url, origin, &mut table));
                        if rewritten != css {
                            let rewritten = rewritten.into_owned();
                            dom.set_text(child, &rewritten);
                        }
                    }
                }
            }
            Some(_) => {}
            None => continue,
        }
        rewrite_attr(&mut dom, node, "background", origin, &mut table);
        if let Some(style) = dom.attr(node, "style") {
            let rewritten = rewrite_css(style, |url| map_url(url, origin, &mut table));
            if rewritten != style {
                let rewritten = rewritten.into_owned();
                dom.set_attr(node, "style", &rewritten);
            }
        }
    }
    RewrittenHtml {
        html: dom.serialize_children(root),
        remote_resources: table.urls,
    }
}

#[derive(Default)]
struct RemoteTable {
    urls: Vec<String>,
    index: HashMap<String, usize>,
}

impl RemoteTable {
    fn number(&mut self, url: String) -> usize {
        if let Some(&n) = self.index.get(&url) {
            return n;
        }
        let n = self.urls.len();
        self.index.insert(url.clone(), n);
        self.urls.push(url);
        n
    }
}

fn map_url(url: &str, origin: &ResourceOrigin, table: &mut RemoteTable) -> Option<String> {
    match classify_resource(url) {
        Resource::Cid(id) => Some(origin.cid_url(&id)),
        Resource::Remote(remote) => Some(origin.remote_url(table.number(remote))),
        Resource::DataImage => Some(url.to_owned()),
        Resource::Rejected => None,
    }
}

fn rewrite_attr(
    dom: &mut Dom,
    node: NodeId,
    attr: &str,
    origin: &ResourceOrigin,
    table: &mut RemoteTable,
) {
    let Some(value) = dom.attr(node, attr) else {
        return;
    };
    match map_url(value, origin, table) {
        Some(new) => dom.set_attr(node, attr, &new),
        None => dom.remove_attr(node, attr),
    }
}

/// The Content-Security-Policy for a document served from `origin`.
///
/// `default-src 'none'; img-src <origin>/ data:; style-src 'unsafe-inline';
/// base-uri 'none'; form-action 'none'`. Images may also be `data:` URLs because the
/// sanitizer only keeps `data:` raster images. The origin gets a trailing slash when it
/// has a path, since a CSP source with a path otherwise matches that exact URL only.
pub fn content_security_policy(origin: &ResourceOrigin) -> String {
    let source = if origin.has_path() {
        format!("{}/", origin.as_str())
    } else {
        origin.as_str().to_owned()
    };
    format!(
        "default-src 'none'; img-src {source} data:; style-src 'unsafe-inline'; \
         base-uri 'none'; form-action 'none'"
    )
}

/// Wrap rewritten HTML into a complete document for a browser: UTF-8, the CSP as a
/// `<meta>` element (serve it as a response header too), and no referrer.
pub fn standalone_document(rewritten: &RewrittenHtml, origin: &ResourceOrigin) -> String {
    let csp = content_security_policy(origin);
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\
         <meta name=\"referrer\" content=\"no-referrer\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         </head><body>\n{}\n</body></html>\n",
        rewritten.html
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> ResourceOrigin {
        ResourceOrigin::new("http://127.0.0.1:4711/tok3n/").unwrap()
    }

    #[test]
    fn validates_origins() {
        assert_eq!(origin().as_str(), "http://127.0.0.1:4711/tok3n");
        assert_eq!(
            ResourceOrigin::new("https://message.mazemail.localhost")
                .unwrap()
                .as_str(),
            "https://message.mazemail.localhost"
        );
        for bad in [
            "message.mazemail.localhost",
            "file:///tmp",
            "javascript:alert(1)",
            "https://a.example; script-src *",
            "https://a.example/?q=1",
            "https://user:pw@a.example",
            "https://a.example/\"x",
        ] {
            assert!(ResourceOrigin::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rewrites_resources() {
        let html = r#"<img src="cid:logo@x"><img src="https://e.com/a.png"><img src="https://e.com/a.png">
            <table><tr><td background="https://e.com/bg.png"></td></tr></table><p style="background:url(cid:bg)">x</p>
            <style>.a{background:url("https://e.com/b.png")}</style><img src="data:image/png;base64,AA">
            <a href="https://e.com/page">link</a>"#;
        let out = rewrite_resources(html, &origin());
        assert_eq!(
            out.remote_resources,
            vec![
                "https://e.com/a.png".to_owned(),
                "https://e.com/bg.png".to_owned(),
                "https://e.com/b.png".to_owned()
            ]
        );
        insta::assert_snapshot!(out.html);
    }

    #[test]
    fn content_ids_cannot_break_out() {
        let out = rewrite_resources(
            "<style>.a{background:url('cid:%3C/style%3E%3Cscript%3Ex')}</style><img src=\"cid:%22%3E%3Cscript%3E\">",
            &origin(),
        );
        assert!(!out.html.contains("<script"), "{}", out.html);
        assert!(!out.html.contains("</style>\")"), "{}", out.html);
    }

    #[test]
    fn builds_csp() {
        assert_eq!(
            content_security_policy(&origin()),
            "default-src 'none'; img-src http://127.0.0.1:4711/tok3n/ data:; \
             style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'"
        );
        let host_only = ResourceOrigin::new("https://message.mazemail.localhost/").unwrap();
        assert!(
            content_security_policy(&host_only)
                .contains("img-src https://message.mazemail.localhost data:")
        );
    }

    #[test]
    fn standalone_document_carries_the_csp() {
        let doc = standalone_document(
            &RewrittenHtml {
                html: "<p>x</p>".into(),
                remote_resources: Vec::new(),
            },
            &origin(),
        );
        assert!(doc.contains("Content-Security-Policy"));
        assert!(doc.contains("<p>x</p>"));
    }

    #[test]
    fn origin_round_trips_through_serde() {
        let json = serde_json::to_string(&origin()).unwrap();
        assert_eq!(json, "\"http://127.0.0.1:4711/tok3n\"");
        let back: ResourceOrigin = serde_json::from_str(&json).unwrap();
        assert_eq!(back, origin());
        assert!(serde_json::from_str::<ResourceOrigin>("\"ftp://x\"").is_err());
    }
}

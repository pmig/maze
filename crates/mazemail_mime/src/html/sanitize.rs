//! The mail sanitizer: an `ammonia` allowlist followed by a policy pass on the DOM.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use ammonia::{Builder, UrlRelative};
use serde::{Deserialize, Serialize};

use super::css::rewrite_css;
use super::dom::{Dom, NodeId};
use super::nesting::limit_nesting;
use super::tracking::TrackerRules;
use super::urls::{Resource, classify_resource, is_allowed_link};

/// Elements that survive sanitizing. Everything else is unwrapped (its children stay)
/// or, for [`CLEAN_CONTENT_TAGS`], removed with its content.
const TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "address",
    "area",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "main",
    "map",
    "mark",
    "nav",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "section",
    "small",
    "span",
    "strike",
    "strong",
    "style",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

/// Elements removed together with everything inside them.
const CLEAN_CONTENT_TAGS: &[&str] = &[
    "applet", "audio", "button", "canvas", "embed", "frame", "frameset", "head", "iframe", "input",
    "math", "noembed", "noframes", "noscript", "object", "option", "script", "select", "svg",
    "template", "textarea", "title", "video", "xmp",
];

/// Attributes allowed on every element.
const GENERIC_ATTRIBUTES: &[&str] = &[
    "align",
    "bgcolor",
    "background",
    "border",
    "class",
    "dir",
    "height",
    "hidden",
    "id",
    "lang",
    "role",
    "style",
    "title",
    "valign",
    "width",
];

/// Attributes allowed on specific elements.
const TAG_ATTRIBUTES: &[(&str, &[&str])] = &[
    ("a", &["href", "name"]),
    ("area", &["alt", "coords", "href", "shape"]),
    ("blockquote", &["cite", "type"]),
    ("col", &["span"]),
    ("colgroup", &["span"]),
    ("del", &["cite", "datetime"]),
    ("font", &["color", "face", "size"]),
    ("img", &["alt", "hspace", "src", "usemap", "vspace"]),
    ("ins", &["cite", "datetime"]),
    ("li", &["value"]),
    ("map", &["name"]),
    ("ol", &["reversed", "start", "type"]),
    ("q", &["cite"]),
    (
        "table",
        &["cellpadding", "cellspacing", "frame", "rules", "summary"],
    ),
    (
        "td",
        &["abbr", "colspan", "headers", "nowrap", "rowspan", "scope"],
    ),
    (
        "th",
        &["abbr", "colspan", "headers", "nowrap", "rowspan", "scope"],
    ),
    ("time", &["datetime"]),
    ("ul", &["type"]),
];

/// URL schemes `ammonia` lets through in `href`/`src`. The policy pass narrows this
/// further per attribute (`data:` only for images, `cid:` only for resources).
const URL_SCHEMES: &[&str] = &["cid", "data", "http", "https", "mailto", "tel"];

static SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::empty();
    builder
        .tags(TAGS.iter().copied().collect())
        .clean_content_tags(CLEAN_CONTENT_TAGS.iter().copied().collect())
        .generic_attributes(GENERIC_ATTRIBUTES.iter().copied().collect())
        .tag_attributes(
            TAG_ATTRIBUTES
                .iter()
                .map(|&(tag, attrs)| (tag, attrs.iter().copied().collect::<HashSet<_>>()))
                .collect::<HashMap<_, _>>(),
        )
        .tag_attribute_values(HashMap::new())
        .generic_attribute_prefixes(HashSet::new())
        .url_schemes(URL_SCHEMES.iter().copied().collect())
        // Relative URLs mean nothing in mail; the policy pass drops them.
        .url_relative(UrlRelative::PassThrough)
        .link_rel(Some("noopener noreferrer"))
        .strip_comments(true)
        .id_prefix(None);
    builder
});

/// Options for [`sanitize`].
#[derive(Debug, Clone, Default)]
pub struct SanitizeOptions {
    /// How tracking pixels are recognized.
    pub trackers: TrackerRules,
}

/// Sanitized message HTML, independent of where it will be shown.
///
/// Resource URLs are still the message's own (`cid:…`, `https://…`, `data:image/…`):
/// [`rewrite_resources`](super::rewrite_resources) maps them to a resource origin for
/// one output. That keeps this value cacheable.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizedHtml {
    /// The sanitized HTML fragment (no `<html>`/`<head>`/`<body>` wrapper).
    pub html: String,
    /// How many tracking pixels were removed.
    pub trackers_removed: usize,
    /// Whether the document references remote (`http(s)`) resources, so the header
    /// bar can offer "load remote content".
    pub has_remote_resources: bool,
    /// Content-IDs referenced by the document, in first-use order, without duplicates.
    pub cid_references: Vec<String>,
}

impl std::fmt::Debug for SanitizedHtml {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SanitizedHtml")
            .field(
                "html",
                &format_args!("<{} bytes redacted>", self.html.len()),
            )
            .field("trackers_removed", &self.trackers_removed)
            .field("has_remote_resources", &self.has_remote_resources)
            .field("cid_references", &self.cid_references.len())
            .finish()
    }
}

/// Sanitize message HTML with the mail allowlist.
///
/// Drops scripts, frames, plugins, forms, `meta`/`base`/`link`, event handlers and
/// every URL that isn't `http(s)`, `mailto:`, `tel:`, `cid:`, a fragment link or a
/// `data:` raster image on an image. Inline `style` attributes and `<style>`
/// elements stay, with `@import` and script hooks neutralized and `url(...)` limited
/// to the same resource rules. Tracking pixels are removed and counted.
pub fn sanitize(html: &str, options: &SanitizeOptions) -> SanitizedHtml {
    let (dom, sanitized) = sanitize_to_dom(html, options);
    let root = dom.root();
    SanitizedHtml {
        html: dom.serialize_children(root).trim().to_owned(),
        ..sanitized
    }
}

/// Sanitize and return the cleaned DOM, for callers that also build the reader model.
pub(crate) fn sanitize_to_dom(html: &str, options: &SanitizeOptions) -> (Dom, SanitizedHtml) {
    let cleaned = SANITIZER.clean(&limit_nesting(html)).to_string();
    let mut dom = Dom::parse_fragment(&cleaned);
    let mut state = PolicyState::default();
    apply_policy(&mut dom, options, &mut state);
    let summary = SanitizedHtml {
        html: String::new(),
        trackers_removed: state.trackers_removed,
        has_remote_resources: state.has_remote,
        cid_references: state.cids,
    };
    (dom, summary)
}

#[derive(Default)]
struct PolicyState {
    trackers_removed: usize,
    has_remote: bool,
    cids: Vec<String>,
    seen_cids: HashSet<String>,
}

impl PolicyState {
    /// Record a resource URL; returns whether it may stay.
    fn note(&mut self, resource: &Resource) -> bool {
        match resource {
            Resource::Cid(id) => {
                if self.seen_cids.insert(id.clone()) {
                    self.cids.push(id.clone());
                }
                true
            }
            Resource::Remote(_) => {
                self.has_remote = true;
                true
            }
            Resource::DataImage => true,
            Resource::Rejected => false,
        }
    }
}

fn apply_policy(dom: &mut Dom, options: &SanitizeOptions, state: &mut PolicyState) {
    let root = dom.root();
    let mut stack: Vec<NodeId> = dom.children(root).iter().rev().copied().collect();
    while let Some(node) = stack.pop() {
        if police_element(dom, node, options, state) == Verdict::Keep {
            stack.extend(dom.children(node).iter().rev().copied());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Keep,
    Removed,
}

fn police_element(
    dom: &mut Dom,
    node: NodeId,
    options: &SanitizeOptions,
    state: &mut PolicyState,
) -> Verdict {
    let Some(tag) = dom.tag(node) else {
        return Verdict::Keep;
    };
    match tag {
        "img" => {
            let resource = dom
                .attr(node, "src")
                .map_or(Resource::Rejected, classify_resource);
            if let Resource::Remote(url) = &resource
                && options.trackers.is_tracking_image(dom, node, url)
            {
                state.trackers_removed += 1;
                dom.detach(node);
                return Verdict::Removed;
            }
            if !state.note(&resource) {
                dom.detach(node);
                return Verdict::Removed;
            }
        }
        "a" | "area" => {
            if dom
                .attr(node, "href")
                .is_some_and(|href| !is_allowed_link(href))
            {
                dom.remove_attr(node, "href");
            }
        }
        "style" => police_style_element(dom, node, state),
        _ => {}
    }
    if dom.attr(node, "cite").is_some_and(|v| !is_allowed_link(v)) {
        dom.remove_attr(node, "cite");
    }
    if let Some(background) = dom.attr(node, "background") {
        let resource = classify_resource(background);
        if !state.note(&resource) {
            dom.remove_attr(node, "background");
        }
    }
    if let Some(style) = dom.attr(node, "style") {
        let policed = police_css(style, state);
        if policed != style {
            dom.set_attr(node, "style", &policed);
        }
    }
    Verdict::Keep
}

fn police_style_element(dom: &mut Dom, node: NodeId, state: &mut PolicyState) {
    for child in dom.children(node).to_vec() {
        if let Some(css) = dom.text(child) {
            let policed = police_css(css, state);
            if policed != css {
                dom.set_text(child, &policed);
            }
        }
    }
}

fn police_css(css: &str, state: &mut PolicyState) -> String {
    rewrite_css(css, |url| {
        let resource = classify_resource(url);
        state.note(&resource).then(|| url.to_owned())
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(html: &str) -> String {
        sanitize(html, &SanitizeOptions::default()).html
    }

    #[test]
    fn drops_active_content() {
        let html = r#"<script>alert(1)</script><p onclick="x()">Hi<iframe src="https://e.com"></iframe></p>
            <object data="x"></object><embed src="x"><form action="https://e.com/steal"><input name=a>text</form>
            <meta http-equiv="refresh" content="0;url=https://e.com"><base href="https://e.com/">
            <link rel="import" href="https://e.com/x"><svg><script>alert(1)</script></svg>"#;
        let out = clean(html);
        assert_eq!(out.trim_end(), "<p>Hi</p>\n            text");
    }

    #[test]
    fn drops_dangerous_urls() {
        let out = clean(
            r#"<a href="javascript:alert(1)">a</a><a href=" JaVaScRiPt:alert(1)">b</a>
            <a href="data:text/html,x">c</a><img src="data:image/svg+xml,x"><img src="relative.png">
            <a href="vbscript:x">d</a><td background="javascript:x">"#,
        );
        assert!(!out.to_ascii_lowercase().contains("script:"), "{out}");
        assert!(!out.contains("data:"), "{out}");
        assert!(!out.contains("<img"), "{out}");
    }

    #[test]
    fn keeps_mail_formatting() {
        let out = clean(
            r##"<table width="600" cellpadding="0" style="color:red"><tr><td bgcolor="eee" align=center>
            <font face="Arial" color="#333">Hi</font> <a href="https://e.com" target="_blank">link</a>
            <img src="cid:logo@x" alt="Logo" width="20" height="20"></td></tr></table>"##,
        );
        assert!(
            out.contains(r#"<table width="600" cellpadding="0" style="color:red">"#),
            "{out}"
        );
        assert!(
            out.contains(r##"<font face="Arial" color="#333">"##),
            "{out}"
        );
        assert!(out.contains(r#"href="https://e.com""#), "{out}");
        assert!(out.contains(r#"rel="noopener noreferrer""#), "{out}");
        assert!(!out.contains("target"), "{out}");
        assert!(out.contains(r#"src="cid:logo@x""#), "{out}");
    }

    #[test]
    fn keeps_and_polices_style() {
        let out = clean(
            "<style>@import url(https://e.com/x.css); .a { background: url(javascript:x) } \
             .b { background: url('cid:bg') }</style><p style=\"background:url(https://e.com/bg.png)\">x</p>",
        );
        assert_eq!(
            out,
            "<style> .a { background: url(\"\") } \
             .b { background: url(\"cid:bg\") }</style><p style=\"background:url(&quot;https://e.com/bg.png&quot;)\">x</p>"
        );
    }

    #[test]
    fn counts_resources_and_trackers() {
        let result = sanitize(
            r#"<img src="cid:a"><img src="cid:a"><img src="https://e.com/x.png">
               <img src="https://e.com/o.gif" width=1 height=1><img src="https://x.awstrack.me/I0/1">"#,
            &SanitizeOptions::default(),
        );
        assert_eq!(result.trackers_removed, 2);
        assert!(result.has_remote_resources);
        assert_eq!(result.cid_references, vec!["a".to_owned()]);
    }

    #[test]
    fn debug_redacts_content() {
        let result = sanitize("<p>secret</p>", &SanitizeOptions::default());
        assert!(!format!("{result:?}").contains("secret"));
    }
}

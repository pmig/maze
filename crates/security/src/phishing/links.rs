//! Link text vs. link target.

use std::fmt;

use mazemail_mime::ReaderDoc;
use mazemail_mime::reader::{Block, Inline};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

use super::{Severity, Warning, WarningKind};
use crate::domain::{registrable_domain, to_unicode};

/// A link as the reader sees it: the visible text and the target.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Link {
    pub text: String,
    pub href: String,
}

impl Link {
    pub fn new(text: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            href: href.into(),
        }
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Link(<redacted>)")
    }
}

/// The links of a reader document, in reading order. Adjacent runs with the same
/// target are joined into one link (`<a><b>pay</b>pal.com</a>` is one link with
/// the text `paypal.com`). Linked images count with their alt text.
pub fn links_from_reader_doc(doc: &ReaderDoc) -> Vec<Link> {
    let mut links = Vec::new();
    collect_blocks(&doc.blocks, &mut links);
    links
}

fn collect_blocks(blocks: &[Block], links: &mut Vec<Link>) {
    for block in blocks {
        match block {
            Block::Paragraph { content } | Block::Heading { content, .. } => {
                collect_inlines(content, links);
            }
            Block::List { items, .. } => {
                for item in items {
                    collect_blocks(&item.blocks, links);
                }
            }
            Block::Quote { blocks, .. } | Block::Signature { blocks } => {
                collect_blocks(blocks, links);
            }
            Block::Table { rows } => {
                for cell in rows.iter().flat_map(|row| &row.cells) {
                    collect_inlines(&cell.content, links);
                }
            }
            Block::Image(image) => {
                if let Some(href) = &image.link {
                    links.push(Link::new(
                        image.alt.clone().unwrap_or_default(),
                        href.clone(),
                    ));
                }
            }
            Block::Preformatted { .. } | Block::Rule => {}
        }
    }
}

fn collect_inlines(inlines: &[Inline], links: &mut Vec<Link>) {
    let mut current: Option<Link> = None;
    for inline in inlines {
        match (&inline.link, current.as_mut()) {
            (Some(href), Some(link)) if *href == link.href => link.text.push_str(&inline.text),
            (Some(href), _) => {
                links.extend(current.take());
                current = Some(Link::new(inline.text.clone(), href.clone()));
            }
            (None, _) => links.extend(current.take()),
        }
    }
    links.extend(current);
}

/// Warn about links whose text shows a domain other than the one they open.
///
/// Only text that is itself a URL or domain counts (`https://paypal.com/login`,
/// `www.paypal.com`, `paypal.com`); "Click here" doesn't. Domains are compared by
/// registrable domain, so `www.example.com` opening `mail.example.com` is fine.
pub fn check_links(links: &[Link]) -> Vec<Warning> {
    let mut warnings: Vec<Warning> = Vec::new();
    for link in links {
        let (Some(text_host), Some(href_host)) = (host_in_text(&link.text), href_host(&link.href))
        else {
            continue;
        };
        let text_domain = registrable_domain(&text_host);
        let href_domain = registrable_domain(&href_host);
        if text_domain == href_domain {
            continue;
        }
        let kind = WarningKind::LinkTextMismatch {
            text_domain: text_domain.to_owned(),
            href_domain: href_domain.to_owned(),
        };
        if warnings.iter().any(|w| w.kind == kind) {
            continue;
        }
        warnings.push(Warning {
            message: format!(
                "A link shows {} but opens {}.",
                to_unicode(text_domain),
                to_unicode(href_domain)
            ),
            kind,
            severity: Severity::Caution,
        });
    }
    warnings
}

/// The distinct hosts of the `http(s)` links, lowercase ASCII.
pub(crate) fn link_hosts(links: &[Link]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for host in links.iter().filter_map(|l| href_host(&l.href)) {
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

/// The host of an `http` or `https` URL, lowercase ASCII (IDNA applied).
fn href_host(href: &str) -> Option<String> {
    let url = Url::parse(href.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    host_string(&url.host()?)
}

fn host_string(host: &Host<&str>) -> Option<String> {
    Some(match host {
        Host::Domain(domain) => domain.trim_end_matches('.').to_ascii_lowercase(),
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    })
}

/// The host shown by link text that looks like a URL or a bare domain.
fn host_in_text(text: &str) -> Option<String> {
    let text = text
        .trim()
        .trim_matches(|c| matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']'))
        .trim_end_matches(['.', ',', ';', ':', '!', '?']);
    if text.is_empty() || text.chars().any(char::is_whitespace) || !text.contains('.') {
        return None;
    }
    let lower = text.to_lowercase();
    let has_scheme = lower.starts_with("http://") || lower.starts_with("https://");
    if !has_scheme && (lower.contains("://") || lower.contains('@') || lower.starts_with("mailto:"))
    {
        return None;
    }
    let candidate = if has_scheme {
        text.to_owned()
    } else {
        format!("http://{text}")
    };
    let url = Url::parse(&candidate).ok()?;
    match url.host()? {
        Host::Domain(domain) => {
            let domain = domain.trim_end_matches('.').to_ascii_lowercase();
            // Bare text needs a real public suffix, so `v1.2` or `report.docx`
            // aren't taken for domains.
            let known = psl::suffix(domain.as_bytes()).is_some_and(|s| s.is_known());
            (has_scheme || (known && psl::domain(domain.as_bytes()).is_some())).then_some(domain)
        }
        host if has_scheme => host_string(&host),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use mazemail_mime::reader::{Image, ImageSource, InlineStyle, ListItem};

    use super::*;

    fn linked(text: &str, href: &str) -> Inline {
        Inline {
            text: text.into(),
            style: InlineStyle::empty(),
            link: Some(href.into()),
        }
    }

    #[test]
    fn extracts_text_hosts() {
        assert_eq!(
            host_in_text("https://www.PayPal.com/login").as_deref(),
            Some("www.paypal.com")
        );
        assert_eq!(host_in_text("paypal.com").as_deref(), Some("paypal.com"));
        assert_eq!(
            host_in_text(" <www.example.co.uk/x>. ").as_deref(),
            Some("www.example.co.uk")
        );
        assert_eq!(
            host_in_text("http://192.0.2.1/").as_deref(),
            Some("192.0.2.1")
        );
        assert_eq!(
            host_in_text("bücher.de").as_deref(),
            Some("xn--bcher-kva.de")
        );
        for text in [
            "Click here",
            "v1.2",
            "1.5",
            "anna@example.com",
            "ftp://example.com",
            "report.docx",
            "",
            "a b.com",
        ] {
            assert_eq!(host_in_text(text), None, "{text}");
        }
    }

    #[test]
    fn flags_mismatched_links() {
        let links = [
            Link::new(
                "www.paypal.com",
                "https://paypal.com.account-check.example/login",
            ),
            Link::new("https://www.paypal.com", "https://evil.example/"),
            Link::new("www.paypal.com", "https://evil.example/other"),
            Link::new("www.example.com", "https://mail.example.com/x"),
            Link::new("Click here", "https://evil.example/"),
            Link::new("example.com", "mailto:someone@example.org"),
            Link::new("example.com", "http://[2001:db8::1]/"),
        ];
        let warnings = check_links(&links);
        let pairs: Vec<_> = warnings
            .iter()
            .map(|w| match &w.kind {
                WarningKind::LinkTextMismatch {
                    text_domain,
                    href_domain,
                } => (text_domain.as_str(), href_domain.as_str()),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            pairs,
            [
                ("paypal.com", "account-check.example"),
                ("paypal.com", "evil.example"),
                ("example.com", "2001:db8::1"),
            ]
        );
        assert!(warnings.iter().all(|w| w.severity == Severity::Caution));
        assert_eq!(
            warnings[1].message,
            "A link shows paypal.com but opens evil.example."
        );
    }

    #[test]
    fn collects_links_from_reader_doc() {
        let doc = ReaderDoc {
            source: mazemail_mime::reader::DocSource::Html,
            blocks: vec![
                Block::Paragraph {
                    content: vec![
                        Inline::plain("Log in at "),
                        linked("pay", "https://evil.example/"),
                        linked("pal.com", "https://evil.example/"),
                        Inline::plain(" or "),
                        linked("here", "https://other.example/"),
                    ],
                },
                Block::List {
                    ordered: false,
                    start: None,
                    items: vec![ListItem {
                        blocks: vec![Block::Quote {
                            depth: 1,
                            blocks: vec![Block::Heading {
                                level: 1,
                                content: vec![linked("deep", "https://deep.example/")],
                            }],
                        }],
                    }],
                },
                Block::Image(Image {
                    source: ImageSource::Remote("https://img.example/x.png".into()),
                    alt: Some("paypal.com".into()),
                    width: None,
                    height: None,
                    link: Some("https://evil.example/img".into()),
                }),
            ],
        };
        let links = links_from_reader_doc(&doc);
        assert_eq!(
            links,
            [
                Link::new("paypal.com", "https://evil.example/"),
                Link::new("here", "https://other.example/"),
                Link::new("deep", "https://deep.example/"),
                Link::new("paypal.com", "https://evil.example/img"),
            ]
        );
        assert_eq!(
            link_hosts(&links),
            ["evil.example", "other.example", "deep.example"]
        );
    }
}

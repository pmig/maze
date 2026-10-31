//! Icon links of a web page (`<link rel="icon">`, `apple-touch-icon`) and its
//! `<base href>`, for favicons fetched from a domain's own website. Nothing is fetched
//! or resolved here; URLs are returned as written.

use super::dom::Dom;

/// What kind of icon a `<link>` declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconRel {
    /// `rel="icon"` or `rel="shortcut icon"`.
    Icon,
    /// `rel="apple-touch-icon"` (or `-precomposed`): usually a 180×180 PNG.
    AppleTouchIcon,
}

/// One icon link of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconLink {
    /// The kind of icon.
    pub rel: IconRel,
    /// `href`, trimmed, unresolved.
    pub href: String,
    /// The largest square size in `sizes` (`32x32 64x64`), if any.
    pub size: Option<u32>,
    /// `sizes="any"` (scalable, usually SVG).
    pub any_size: bool,
    /// `type`, lowercase, if given (`image/png`).
    pub mime: Option<String>,
}

/// The icon links of a page, in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageIcons {
    /// The first `<base href>`, unresolved.
    pub base: Option<String>,
    /// Every `<link>` with an icon rel and an `href`. `mask-icon` (a monochrome Safari
    /// pinned-tab SVG) is left out.
    pub links: Vec<IconLink>,
}

/// Finds the icon links and the base URL of an HTML page.
pub fn page_icons(html: &str) -> PageIcons {
    let dom = Dom::parse_fragment(html);
    let mut icons = PageIcons::default();
    for id in dom.descendants(dom.root()) {
        match dom.tag(id) {
            Some("base") if icons.base.is_none() => {
                if let Some(href) = dom.attr(id, "href").map(str::trim)
                    && !href.is_empty()
                {
                    icons.base = Some(href.to_owned());
                }
            }
            Some("link") => {
                let Some(rel) = dom.attr(id, "rel").and_then(icon_rel) else {
                    continue;
                };
                let Some(href) = dom.attr(id, "href").map(str::trim) else {
                    continue;
                };
                if href.is_empty() {
                    continue;
                }
                let sizes = dom.attr(id, "sizes").unwrap_or("");
                icons.links.push(IconLink {
                    rel,
                    href: href.to_owned(),
                    size: largest_size(sizes),
                    any_size: sizes
                        .split_ascii_whitespace()
                        .any(|s| s.eq_ignore_ascii_case("any")),
                    mime: dom
                        .attr(id, "type")
                        .map(|t| t.trim().to_ascii_lowercase())
                        .filter(|t| !t.is_empty()),
                });
            }
            _ => {}
        }
    }
    icons
}

fn icon_rel(rel: &str) -> Option<IconRel> {
    let mut found = None;
    for token in rel.split_ascii_whitespace() {
        let token = token.to_ascii_lowercase();
        match token.as_str() {
            "apple-touch-icon" | "apple-touch-icon-precomposed" => {
                return Some(IconRel::AppleTouchIcon);
            }
            "icon" => found = Some(IconRel::Icon),
            "mask-icon" => return None,
            _ => {}
        }
    }
    found
}

/// The largest `WxH` in a `sizes` attribute, as the smaller of the two sides.
fn largest_size(sizes: &str) -> Option<u32> {
    sizes
        .split_ascii_whitespace()
        .filter_map(|size| {
            let (w, h) = size
                .to_ascii_lowercase()
                .split_once('x')
                .map(|(w, h)| (w.parse::<u32>().ok(), h.parse::<u32>().ok()))?;
            Some(w?.min(h?))
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_icon_links_and_base() {
        let html = r##"<!doctype html><html><head>
            <base href="https://cdn.northwind.example/assets/">
            <link rel="stylesheet" href="/style.css">
            <link rel="shortcut icon" href=" /favicon.ico ">
            <link rel="icon" type="image/png" sizes="16x16 32X32" href="icon-32.png">
            <link rel="apple-touch-icon" href="/apple.png">
            <link rel="icon" sizes="any" type="IMAGE/SVG+XML" href="/icon.svg">
            <link rel="mask-icon" href="/mask.svg" color="#000">
            <link rel="icon" href="">
            <script>document.write('<link rel="icon" href="/evil.png">')</script>
            </head><body><link rel="icon" href="/late.png"></body></html>"##;
        let icons = page_icons(html);
        assert_eq!(
            icons.base.as_deref(),
            Some("https://cdn.northwind.example/assets/")
        );
        assert_eq!(
            icons.links,
            vec![
                IconLink {
                    rel: IconRel::Icon,
                    href: "/favicon.ico".into(),
                    size: None,
                    any_size: false,
                    mime: None
                },
                IconLink {
                    rel: IconRel::Icon,
                    href: "icon-32.png".into(),
                    size: Some(32),
                    any_size: false,
                    mime: Some("image/png".into())
                },
                IconLink {
                    rel: IconRel::AppleTouchIcon,
                    href: "/apple.png".into(),
                    size: None,
                    any_size: false,
                    mime: None
                },
                IconLink {
                    rel: IconRel::Icon,
                    href: "/icon.svg".into(),
                    size: None,
                    any_size: true,
                    mime: Some("image/svg+xml".into())
                },
                IconLink {
                    rel: IconRel::Icon,
                    href: "/late.png".into(),
                    size: None,
                    any_size: false,
                    mime: None
                },
            ]
        );
    }

    #[test]
    fn pages_without_icons() {
        assert_eq!(page_icons(""), PageIcons::default());
        assert_eq!(page_icons("not html at all"), PageIcons::default());
        assert_eq!(largest_size("axb 12x"), None);
        assert_eq!(largest_size("48x32"), Some(32));
    }
}

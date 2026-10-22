//! Tracking-pixel detection.

use url::Url;

use super::dom::{Dom, NodeId};

/// Hosts that serve open-tracking images. A host matches itself and its subdomains.
const TRACKER_HOSTS: &[&str] = &[
    "awstrack.me",
    "app.bananatag.com",
    "eotrx.substackcdn.com",
    "getnotify.com",
    "mailfoogae.appspot.com",
    "mailstat.us",
    "mailtrack.io",
    "mltrk.io",
    "pixel.app.returnpath.net",
    "r.superhuman.com",
    "t.sidekickopen.com",
    "t.signaux.com",
    "t.senal.com",
    "t.yesware.com",
];

/// Path fragments of open-tracking endpoints used by bulk senders on custom domains
/// (Mailchimp and Mandrill `/track/open`, SendGrid `/wf/open`, Salesforce
/// `/open.aspx`, Constant Contact `/on.jsp`).
const TRACKER_PATHS: &[&str] = &["/track/open", "/wf/open", "/open.aspx", "/on.jsp"];

/// Rules for recognizing tracking pixels.
///
/// The default rules flag remote images that are tiny (at most 1×1, or 0 in either
/// dimension), hidden with inline CSS, or served by a known tracker host or tracking
/// endpoint. The lists are short on purpose and can be extended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerRules {
    hosts: Vec<String>,
    path_markers: Vec<String>,
}

impl Default for TrackerRules {
    fn default() -> Self {
        Self {
            hosts: TRACKER_HOSTS.iter().map(|&h| h.to_owned()).collect(),
            path_markers: TRACKER_PATHS.iter().map(|&p| p.to_owned()).collect(),
        }
    }
}

impl TrackerRules {
    /// Rules without any known hosts or paths; only size and visibility checks apply.
    pub fn without_known_trackers() -> Self {
        Self {
            hosts: Vec::new(),
            path_markers: Vec::new(),
        }
    }

    /// Add a tracker host (matches the host and all its subdomains).
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.hosts.push(host.into().to_ascii_lowercase());
        self
    }

    /// Add a path fragment that marks a tracking endpoint (case-insensitive).
    #[must_use]
    pub fn with_path_marker(mut self, marker: impl Into<String>) -> Self {
        self.path_markers.push(marker.into().to_ascii_lowercase());
        self
    }

    /// Whether a remote URL belongs to a known tracker.
    pub fn is_tracker_url(&self, url: &str) -> bool {
        let Ok(url) = Url::parse(url) else {
            return false;
        };
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let host_match = self.hosts.iter().any(|known| {
            host == *known
                || host
                    .strip_suffix(known.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.'))
        });
        if host_match {
            return true;
        }
        let path = url.path().to_ascii_lowercase();
        self.path_markers
            .iter()
            .any(|marker| path.contains(marker.as_str()))
    }

    /// Whether an `<img>` with a remote `src` is a tracking pixel.
    pub(crate) fn is_tracking_image(&self, dom: &Dom, img: NodeId, remote_url: &str) -> bool {
        if self.is_tracker_url(remote_url) {
            return true;
        }
        let style = dom.attr(img, "style").map(parse_style).unwrap_or_default();
        let style_value = |name: &str| {
            style
                .iter()
                .rev()
                .find(|(prop, _)| prop == name)
                .map(|(_, value)| value.as_str())
        };
        let width = dom
            .attr(img, "width")
            .and_then(parse_pixels)
            .or_else(|| style_value("width").and_then(parse_pixels));
        let height = dom
            .attr(img, "height")
            .and_then(parse_pixels)
            .or_else(|| style_value("height").and_then(parse_pixels));
        let tiny = match (width, height) {
            (Some(w), Some(h)) => (w <= 1 && h <= 1) || w == 0 || h == 0,
            (Some(0), None) | (None, Some(0)) => true,
            _ => false,
        };
        let hidden = style_value("display") == Some("none")
            || style_value("visibility") == Some("hidden")
            || style_value("opacity").and_then(|o| o.parse::<f32>().ok()) == Some(0.0);
        tiny || hidden
    }
}

/// Split an inline style into lowercase `(property, value)` pairs.
pub(crate) fn parse_style(style: &str) -> Vec<(String, String)> {
    style
        .split(';')
        .filter_map(|decl| {
            let (prop, value) = decl.split_once(':')?;
            let value = value.trim().trim_end_matches("!important").trim();
            Some((prop.trim().to_ascii_lowercase(), value.to_ascii_lowercase()))
        })
        .collect()
}

/// Parse `1`, `1px` or `1.0px` into whole pixels. Percentages and other units are
/// not pixel sizes and yield `None`.
pub(crate) fn parse_pixels(value: &str) -> Option<u32> {
    let value = value.trim();
    let number = value.strip_suffix("px").unwrap_or(value).trim();
    let parsed: f64 = number.parse().ok()?;
    (parsed.is_finite() && parsed >= 0.0).then(|| {
        // Saturating float-to-int conversion; sizes are small, positive numbers.
        parsed.round().min(f64::from(u32::MAX)) as u32
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_img(dom: &Dom) -> NodeId {
        dom.descendants(dom.root())
            .into_iter()
            .find(|&n| dom.tag(n) == Some("img"))
            .unwrap()
    }

    fn is_tracker(html: &str) -> bool {
        let dom = Dom::parse_fragment(html);
        let img = first_img(&dom);
        let src = dom.attr(img, "src").unwrap().to_owned();
        TrackerRules::default().is_tracking_image(&dom, img, &src)
    }

    #[test]
    fn detects_tiny_and_hidden_images() {
        assert!(is_tracker(
            r#"<img src="https://e.com/p.gif" width="1" height="1">"#
        ));
        assert!(is_tracker(r#"<img src="https://e.com/p.gif" width="0">"#));
        assert!(is_tracker(
            r#"<img src="https://e.com/p.gif" style="width:1px;height:1px">"#
        ));
        assert!(is_tracker(
            r#"<img src="https://e.com/p.gif" style="display: none">"#
        ));
        assert!(!is_tracker(
            r#"<img src="https://e.com/logo.png" width="120" height="1">"#
        ));
        assert!(!is_tracker(r#"<img src="https://e.com/logo.png">"#));
        assert!(!is_tracker(
            r#"<img src="https://e.com/logo.png" width="100%" height="1">"#
        ));
    }

    #[test]
    fn detects_known_trackers() {
        let rules = TrackerRules::default();
        assert!(rules.is_tracker_url("https://abc.r.us-east-1.awstrack.me/I0/x"));
        assert!(rules.is_tracker_url("https://news.example.com/track/open.php?u=1"));
        assert!(rules.is_tracker_url("https://u123.ct.sendgrid.net/wf/open?upn=x"));
        assert!(!rules.is_tracker_url("https://notawstrack.me/logo.png"));
        assert!(!rules.is_tracker_url("https://example.com/logo.png"));
        let custom = TrackerRules::without_known_trackers().with_host("Pixel.Example");
        assert!(custom.is_tracker_url("https://a.pixel.example/x.gif"));
        assert!(!custom.is_tracker_url("https://abc.awstrack.me/I0/x"));
    }

    #[test]
    fn parses_pixel_values() {
        assert_eq!(parse_pixels("1"), Some(1));
        assert_eq!(parse_pixels(" 2px "), Some(2));
        assert_eq!(parse_pixels("0.4"), Some(0));
        assert_eq!(parse_pixels("50%"), None);
        assert_eq!(parse_pixels("-1"), None);
        assert_eq!(parse_pixels("NaN"), None);
    }
}

//! Links in a message: where a hovered link really goes, and what a click on one does.
//!
//! # The preview
//!
//! A small pill at the bottom left of the message shows the hovered link's target, the
//! way browsers do ([`LinkPreview`]):
//!
//! - The target (`href`), never the text: `mailto:` links as their addresses, long URLs
//!   shortened in the middle so the host stays readable ([`display_target`]).
//! - A deceptive link, whose text shows another domain than the one it opens, turns
//!   the pill into a warning. The source decides that with the phishing check of the
//!   message banner ([`BodyLink::deception`]).
//! - [`MessageView::hover_link`](crate::message_view::MessageView::hover_link) is the
//!   hook for any body renderer: call it with the hovered target, or `None` when the
//!   pointer leaves the link.
//!
//! gpui-kit's `TextView` reports clicks on links but not hovering, so
//! [`TextViewLinkProbe`] finds the link under the pointer by asking the view what a
//! click there would open, without opening it.
//!
//! # Clicks
//!
//! [`link_click`] is the policy, [`open_link`] carries it out; any body renderer calls
//! the latter for a click:
//!
//! - The target is normalized ([`normalize_link`]): the system only opens strictly
//!   valid URLs, and mail is full of spaces, non-ASCII paths and `|`.
//! - `http` and `https` open in the browser, `mailto:` opens a draft
//!   ([`ComposeTo`]); every other scheme (`javascript:`, `file:`, `data:`, …) is
//!   refused with a notification.
//! - A deceptive link asks first ("This link goes to evil.example, not paypal.com").
//! - A target that isn't a URL shows a notification with the link and a Copy button.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use gpui::{
    App, ClickEvent, ClipboardItem, InteractiveElement as _, IntoElement, MouseButton,
    MouseUpEvent, ParentElement as _, Pixels, PlatformInput, Point, PromptLevel, RenderOnce,
    SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, relative,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, WindowExt as _, button::Button, h_flex,
    notification::Notification, text::TextView, v_flex,
};
use mazemail_types::PRODUCT_NAME;
use ui::assets::IconName;
use url::{Position, Url};

use crate::actions::ComposeTo;
use crate::source::{BodyLink, Deception};

/// Characters a link target shows at most before it is shortened in the middle.
pub const MAX_TARGET_CHARS: usize = 90;

/// What kind of target a link has, for the pill's icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// A web page (`http`, `https`).
    Web,
    /// A `mailto:` link: the pill shows the addresses.
    Mail,
    /// Anything else (`tel:`, `ftp:`, …), shown as written.
    Other,
}

/// A hovered link as the preview shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct HoveredLink {
    href: SharedString,
    target: SharedString,
    kind: LinkKind,
    warning: Option<SharedString>,
}

impl std::fmt::Debug for HoveredLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HoveredLink")
            .field("kind", &self.kind)
            .field("deceptive", &self.warning.is_some())
            .finish_non_exhaustive()
    }
}

impl HoveredLink {
    /// The preview of `href`, with a warning when the link is deceptive.
    pub fn new(href: &str, deception: Option<&Deception>) -> Self {
        let (target, kind) = display_target(href, MAX_TARGET_CHARS);
        Self {
            href: href.trim().to_owned().into(),
            target: target.into(),
            kind,
            warning: deception
                .map(|d| format!("This link shows {} but opens {}.", d.shown, d.opens).into()),
        }
    }

    /// The preview of `href` among a body's `links` (see [`deception_of`]).
    pub fn resolve(href: &str, links: &[BodyLink]) -> Self {
        Self::new(href, deception_of(href, links))
    }

    /// The link's target as written in the message.
    pub fn href(&self) -> &SharedString {
        &self.href
    }

    /// What the pill shows: the target, shortened (see [`display_target`]).
    pub fn target(&self) -> &SharedString {
        &self.target
    }

    /// The kind of target.
    pub fn kind(&self) -> LinkKind {
        self.kind
    }

    /// Why the link is deceptive, if it is.
    pub fn warning(&self) -> Option<&SharedString> {
        self.warning.as_ref()
    }
}

/// Whether the link to `href` is deceptive, among a body's `links`: when any link of
/// the body with this target is (the same target can sit behind several texts).
pub fn deception_of<'a>(href: &str, links: &'a [BodyLink]) -> Option<&'a Deception> {
    let href = href.trim();
    links
        .iter()
        .filter(|link| link.href.trim() == href)
        .find_map(|link| link.deception.as_ref())
}

/// How a link target reads in the preview, at most `max_chars` long (plus the host):
///
/// - `mailto:anna@example.com?subject=Hi` shows `anna@example.com` (several addresses
///   are separated by commas).
/// - Web links show scheme and host in full, then as much of the path, query and
///   fragment as fits, cut in the middle: `https://example.com/a/v…/report.pdf`.
///   User names and passwords before the host (`https://paypal.com@evil.example/`)
///   are left out, so the real host is what you read.
/// - Anything else shows as written, cut in the middle when too long.
pub fn display_target(href: &str, max_chars: usize) -> (String, LinkKind) {
    let href = href.trim();
    let Ok(url) = Url::parse(href) else {
        return (truncate_middle("", href, max_chars), LinkKind::Other);
    };
    match url.scheme() {
        "mailto" => {
            let addresses = percent_decode(url.path())
                .split(',')
                .map(str::trim)
                .filter(|address| !address.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            (truncate_middle("", &addresses, max_chars), LinkKind::Mail)
        }
        "http" | "https" if url.host_str().is_some() => {
            let prefix = &url[..Position::BeforeUsername];
            let host = &url[Position::BeforeHost..Position::AfterPort];
            let rest = &url[Position::BeforePath..];
            (
                truncate_middle(&format!("{prefix}{host}"), rest, max_chars),
                LinkKind::Web,
            )
        }
        _ => (truncate_middle("", href, max_chars), LinkKind::Other),
    }
}

/// `prefix` and `rest`, with the middle of `rest` replaced by "…" when both together
/// are longer than `max_chars`. The prefix is always kept whole; the end of `rest`
/// (a file name, the query) gets two thirds of the room.
fn truncate_middle(prefix: &str, rest: &str, max_chars: usize) -> String {
    let prefix_len = prefix.chars().count();
    let rest_len = rest.chars().count();
    if prefix_len + rest_len <= max_chars {
        return format!("{prefix}{rest}");
    }
    let budget = max_chars.saturating_sub(prefix_len + 1);
    let head = budget / 3;
    let tail = budget - head;
    let mut out = String::with_capacity(prefix.len() + budget * 4 + 3);
    out.push_str(prefix);
    out.extend(rest.chars().take(head));
    out.push('…');
    out.extend(rest.chars().skip(rest_len - tail));
    out
}

/// Decodes `%XX` escapes (invalid UTF-8 becomes U+FFFD).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut ix = 0;
    while ix < bytes.len() {
        let hex = |b: u8| char::from(b).to_digit(16);
        if bytes[ix] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(ix + 1).copied().and_then(hex),
                bytes.get(ix + 2).copied().and_then(hex),
            )
        {
            out.push((hi * 16 + lo) as u8);
            ix += 3;
        } else {
            out.push(bytes[ix]);
            ix += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The links in plain text: `http(s)://…`, `www.…` (opened as `https://`) and
/// `mailto:…`, as byte ranges with their targets. Trailing punctuation (`.`, `,`, a
/// closing bracket without an opening one, …) is not part of a link.
pub fn detect_links(text: &str) -> Vec<(Range<usize>, SharedString)> {
    // ASCII lowercasing keeps byte offsets.
    let lower = text.to_ascii_lowercase();
    let mut links = Vec::new();
    let mut ix = 0;
    while ix < text.len() {
        let Some(start) = next_link_start(text, &lower, ix) else {
            break;
        };
        let end = text[start..]
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
            .map_or(text.len(), |len| start + len);
        let end = start + trim_link_end(&text[start..end]);
        let candidate = &text[start..end];
        let href = if lower[start..end].starts_with("www.") {
            format!("https://{candidate}")
        } else {
            candidate.to_owned()
        };
        // A bare scheme ("see https://") or "www." alone is no link.
        let valid = Url::parse(&href).is_ok_and(|url| match url.scheme() {
            "mailto" => url.path().contains('@'),
            _ => url.host_str().is_some_and(|host| host.contains('.')),
        });
        if valid {
            links.push((start..end, href.into()));
        }
        ix = end.max(start + 1);
        while !text.is_char_boundary(ix) {
            ix += 1;
        }
    }
    links
}

/// Where the next link starts at or after `from`: a scheme or `www.` at the start of a
/// word. `lower` is `text` in ASCII lowercase.
fn next_link_start(text: &str, lower: &str, from: usize) -> Option<usize> {
    const STARTS: [&str; 4] = ["https://", "http://", "mailto:", "www."];
    STARTS
        .iter()
        .filter_map(|start| {
            let mut search = from;
            loop {
                let found = search + lower[search..].find(start)?;
                let word_start = text[..found]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric() && !matches!(c, '.' | '/' | '@'));
                if word_start {
                    return Some(found);
                }
                search = found + 1;
            }
        })
        .min()
}

/// The length of `link` without trailing punctuation.
fn trim_link_end(link: &str) -> usize {
    let mut end = link.len();
    loop {
        let Some(last) = link[..end].chars().next_back() else {
            return end;
        };
        let unbalanced = |open: char, close: char| {
            last == close && link[..end].matches(close).count() > link[..end].matches(open).count()
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
            || unbalanced('{', '}')
        {
            end -= last.len_utf8();
        } else {
            return end;
        }
    }
}

/// Why a link isn't opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    /// A scheme Maze doesn't hand on (`javascript`, `file`, `data`, an app's own).
    #[error("{PRODUCT_NAME} doesn't open {0} links.")]
    Scheme(String),
    /// Not a URL at all.
    #[error("This link can't be opened: {0}")]
    Invalid(String),
}

/// The addresses and fields of a `mailto:` link (RFC 6068).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailTo {
    /// Recipients: the link's addresses, then its `to=` fields.
    pub to: Vec<String>,
    /// `cc=`.
    pub cc: Vec<String>,
    /// `bcc=`.
    pub bcc: Vec<String>,
    /// `subject=`.
    pub subject: Option<String>,
    /// `body=`.
    pub body: Option<String>,
}

impl MailTo {
    /// The fields of a `mailto:` URL.
    pub fn from_url(url: &Url) -> Self {
        let addresses = |text: &str| -> Vec<String> {
            percent_decode(text)
                .split(',')
                .map(str::trim)
                .filter(|address| !address.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let mut mailto = Self {
            to: addresses(url.path()),
            ..Self::default()
        };
        for field in url.query().unwrap_or_default().split('&') {
            let (key, value) = field.split_once('=').unwrap_or((field, ""));
            match key.to_ascii_lowercase().as_str() {
                "to" => mailto.to.extend(addresses(value)),
                "cc" => mailto.cc.extend(addresses(value)),
                "bcc" => mailto.bcc.extend(addresses(value)),
                "subject" => mailto.subject = Some(percent_decode(value)),
                "body" => mailto.body = Some(percent_decode(value)),
                _ => {}
            }
        }
        mailto
    }
}

/// What a click on a link does ([`link_click`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkClick {
    /// Open the (normalized) URL in the browser.
    Open(Url),
    /// Write to someone: a `mailto:` link.
    Compose(MailTo),
    /// The link's text shows another domain: ask before opening.
    Confirm {
        /// The normalized URL.
        url: Url,
        /// The host the link opens.
        opens: String,
        /// The domain its text shows.
        shown: SharedString,
    },
    /// Don't open it.
    Refuse(LinkError),
}

/// Characters RFC 3986 allows in a URL after the host, besides ASCII letters and
/// digits. `url` follows the WHATWG URL standard, which leaves `|`, `^`, `[`, `]` and
/// a few more as they are; macOS refuses URLs with them.
const URL_SAFE: &[u8] = b"-._~:/?#@!$&'()*+,;=%";

/// The URL a link opens, normalized so the system takes it: whitespace trimmed, spaces,
/// non-ASCII and other characters a URL can't contain percent-encoded (what is
/// encoded already stays as it is), international hosts in punycode. A link without
/// a scheme that starts with `www.` is taken as `https://`.
pub fn normalize_link(href: &str) -> Result<Url, LinkError> {
    let href = href.trim();
    let invalid = || LinkError::Invalid(href.to_owned());
    let url = match Url::parse(href) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase)
            if href
                .get(..4)
                .is_some_and(|s| s.eq_ignore_ascii_case("www.")) =>
        {
            Url::parse(&format!("https://{href}")).map_err(|_| invalid())?
        }
        Err(_) => return Err(invalid()),
    };
    if url.cannot_be_a_base() && url.scheme() != "mailto" {
        // `javascript:…`, `data:…`: nothing to encode, the scheme decides.
        return Ok(url);
    }
    let (start, rest) = url.as_str().split_at(url[..Position::AfterPort].len());
    let mut encoded = String::with_capacity(url.as_str().len());
    encoded.push_str(start);
    for byte in rest.bytes() {
        if byte.is_ascii_alphanumeric() || URL_SAFE.contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    Url::parse(&encoded).map_err(|_| invalid())
}

/// What a click on the link to `href` does, among the body's `links` (which tell
/// whether it's deceptive). The reusable policy behind [`open_link`].
pub fn link_click(href: &str, links: &[BodyLink]) -> LinkClick {
    let url = match normalize_link(href) {
        Ok(url) => url,
        Err(error) => return LinkClick::Refuse(error),
    };
    match url.scheme() {
        "http" | "https" if url.host_str().is_some() => match deception_of(href, links) {
            Some(deception) => LinkClick::Confirm {
                opens: url
                    .host_str()
                    .unwrap_or_default()
                    .trim_end_matches('.')
                    .to_owned(),
                shown: deception.shown.clone(),
                url,
            },
            None => LinkClick::Open(url),
        },
        "http" | "https" => LinkClick::Refuse(LinkError::Invalid(href.trim().to_owned())),
        "mailto" => LinkClick::Compose(MailTo::from_url(&url)),
        scheme => LinkClick::Refuse(LinkError::Scheme(scheme.to_owned())),
    }
}

/// Opens the link to `href` that was clicked in the shown message, following
/// [`link_click`]: the browser, a draft, a confirmation first, or a notification why
/// not.
pub fn open_link(href: &str, links: &[BodyLink], window: &mut Window, cx: &mut App) {
    match link_click(href, links) {
        LinkClick::Open(url) => cx.open_url(url.as_str()),
        LinkClick::Compose(mailto) => window.dispatch_action(Box::new(ComposeTo(mailto)), cx),
        LinkClick::Confirm { url, opens, shown } => {
            let answer = window.prompt(
                PromptLevel::Warning,
                &format!("This link goes to {opens}, not {shown}."),
                Some("Open anyway?"),
                &[CANCEL, OPEN_ANYWAY],
                cx,
            );
            window
                .spawn(cx, async move |cx| {
                    if answer.await == Ok(1) {
                        cx.update(|_, cx| cx.open_url(url.as_str())).ok();
                    }
                })
                .detach();
        }
        LinkClick::Refuse(error @ LinkError::Scheme(_)) => {
            window.push_notification(
                Notification::warning(error.to_string()).title(PRODUCT_NAME),
                cx,
            );
        }
        LinkClick::Refuse(error @ LinkError::Invalid(_)) => {
            let raw = SharedString::from(href.trim().to_owned());
            window.push_notification(
                Notification::error(error.to_string())
                    .title(PRODUCT_NAME)
                    .action(move |_, _, _| {
                        let raw = raw.clone();
                        Button::new("copy-link")
                            .label("Copy")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(raw.to_string()));
                            })
                    }),
                cx,
            );
        }
    }
}

/// The buttons of the deceptive-link question; "Cancel" first, so it's the default.
const CANCEL: &str = "Cancel";
const OPEN_ANYWAY: &str = "Open Anyway";

/// Finds the link under the pointer in a gpui-kit `TextView`.
///
/// `TextView` has no hover callback for links, only [`TextView::on_link_click`]. The
/// probe installs that callback ([`Self::attach`]) and, for a pointer position, replays
/// a mouse-up there ([`Self::link_at`]): the view hit-tests its laid-out text and
/// reports the link it would open, which the probe records instead of opening it.
/// While probing, the element around the view must stop the replayed event from
/// reaching anything else ([`Self::is_probing`]). Real clicks go to the handler given
/// to [`Self::attach`].
#[derive(Clone, Default)]
pub struct TextViewLinkProbe(Arc<ProbeState>);

#[derive(Default)]
struct ProbeState {
    probing: AtomicBool,
    found: Mutex<Option<SharedString>>,
}

impl std::fmt::Debug for TextViewLinkProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextViewLinkProbe")
            .field("probing", &self.is_probing())
            .finish()
    }
}

impl TextViewLinkProbe {
    /// Lets the probe see `view`'s links. A click that opens a link (left or middle
    /// button, with or without modifiers, the keyboard, a tap; not the right button)
    /// calls `on_click` with its target.
    pub fn attach(
        &self,
        view: TextView,
        on_click: impl Fn(&str, &mut Window, &mut App) + Send + Sync + 'static,
    ) -> TextView {
        let state = self.0.clone();
        view.on_link_click(move |url, event, window, cx| {
            if state.probing.load(Ordering::SeqCst) {
                *state.found.lock().unwrap_or_else(PoisonError::into_inner) = Some(url.clone());
            } else if opens_link(event) {
                on_click(url, window, cx);
            }
        })
    }

    /// Whether a replayed mouse-up is being dispatched: the element around the view
    /// stops its propagation.
    pub fn is_probing(&self) -> bool {
        self.0.probing.load(Ordering::SeqCst)
    }

    /// The target of the link at `position` in an attached view, if there is one.
    ///
    /// Call it outside of event dispatch (from [`Window::defer`]), and only while no
    /// mouse button is pressed, so the replayed mouse-up can't end a drag or a click.
    pub fn link_at(
        &self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<SharedString> {
        let state = &self.0;
        state
            .found
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        state.probing.store(true, Ordering::SeqCst);
        window.dispatch_event(
            PlatformInput::MouseUp(MouseUpEvent {
                button: MouseButton::Left,
                position,
                modifiers: window.modifiers(),
                click_count: 1,
            }),
            cx,
        );
        state.probing.store(false, Ordering::SeqCst);
        state
            .found
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

/// Whether a click opens a link: left (cmd-click too) or middle button, the keyboard,
/// a tap. Not the right button, which opens the context menu.
fn opens_link(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => {
            matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
        }
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    }
}

/// The pill at the bottom left of the message with the hovered link's target. Place
/// it in a `relative()` container; it takes no mouse events.
#[derive(Debug, IntoElement)]
pub struct LinkPreview {
    link: HoveredLink,
}

impl LinkPreview {
    /// The pill for `link`.
    pub fn new(link: HoveredLink) -> Self {
        Self { link }
    }
}

impl RenderOnce for LinkPreview {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let link = self.link;
        let target = h_flex()
            .min_w_0()
            .gap_1()
            .when(link.kind == LinkKind::Mail, |this| {
                this.child(
                    Icon::new(IconName::Mail)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
            })
            .child(div().min_w_0().truncate().child(link.target.clone()));
        v_flex()
            .debug_selector(|| "link-preview".to_owned())
            .absolute()
            .left_1()
            .bottom_1()
            .max_w(relative(0.9))
            .px_2()
            .py_0p5()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .shadow_sm()
            .text_xs()
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .map(|this| match &link.warning {
                Some(warning) => this
                    .border_color(theme.danger)
                    .child(
                        h_flex()
                            .debug_selector(|| "link-preview-warning".to_owned())
                            .min_w_0()
                            .gap_1()
                            .text_color(theme.danger)
                            .font_semibold()
                            .child(Icon::new(IconName::ShieldAlert).xsmall())
                            .child(div().min_w_0().truncate().child(warning.clone())),
                    )
                    .child(target),
                None => this.border_color(theme.border).child(target),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(href: &str) -> String {
        display_target(href, MAX_TARGET_CHARS).0
    }

    #[test]
    fn shows_the_href_not_the_text() {
        let links = [BodyLink {
            text: "Click here".into(),
            href: "https://example.com/account".into(),
            deception: None,
        }];
        let link = HoveredLink::resolve(" https://example.com/account ", &links);
        assert_eq!(link.target().as_ref(), "https://example.com/account");
        assert_eq!(link.href().as_ref(), "https://example.com/account");
        assert_eq!(link.kind(), LinkKind::Web);
        assert_eq!(link.warning(), None);
    }

    #[test]
    fn mailto_shows_the_addresses() {
        assert_eq!(
            display_target("mailto:anna@example.com?subject=Hi%20there", 90),
            ("anna@example.com".to_owned(), LinkKind::Mail)
        );
        assert_eq!(
            target("MAILTO:anna%40example.com,%20ben@example.org"),
            "anna@example.com, ben@example.org"
        );
    }

    #[test]
    fn long_urls_are_cut_in_the_middle_keeping_the_host() {
        let href = format!(
            "https://www.billing.example.com/{}/invoice.pdf?id=4711",
            "a/".repeat(60)
        );
        let shown = display_target(&href, 60).0;
        assert_eq!(shown.chars().count(), 60);
        assert!(
            shown.starts_with("https://www.billing.example.com/a/a/a"),
            "{shown}"
        );
        assert!(shown.ends_with("invoice.pdf?id=4711"), "{shown}");
        assert!(shown.contains('…'));
        // The host stays whole even when it alone is longer than the limit.
        let long_host = format!("https://{}.example.com/path/to/page", "x".repeat(80));
        let shown = display_target(&long_host, 40).0;
        assert!(shown.starts_with(&format!("https://{}.example.com", "x".repeat(80))));
        assert!(shown.ends_with('…'));
        // Short URLs stay as they are.
        assert_eq!(target("https://example.com/"), "https://example.com/");
    }

    #[test]
    fn user_info_before_the_host_is_dropped() {
        assert_eq!(
            target("https://www.paypal.com:secure@evil.example:8443/login"),
            "https://evil.example:8443/login"
        );
    }

    #[test]
    fn other_targets_show_as_written() {
        assert_eq!(
            display_target("tel:+43 1 234", 90),
            ("tel:+43 1 234".to_owned(), LinkKind::Other)
        );
        assert_eq!(display_target("not a url", 90).1, LinkKind::Other);
        let shown = display_target(&format!("ftp://{}", "y".repeat(100)), 20).0;
        assert_eq!(shown.chars().count(), 20);
        assert!(shown.starts_with("ftp://"));
    }

    fn paypal() -> Deception {
        Deception {
            shown: "paypal.com".into(),
            opens: "evil.example".into(),
        }
    }

    #[test]
    fn deceptive_links_carry_the_warning() {
        let links = [
            BodyLink {
                text: "here".into(),
                href: "https://evil.example/login".into(),
                deception: None,
            },
            BodyLink {
                text: "www.paypal.com".into(),
                href: "https://evil.example/login".into(),
                deception: Some(paypal()),
            },
        ];
        let link = HoveredLink::resolve("https://evil.example/login", &links);
        assert_eq!(
            link.warning().map(|w| w.as_ref()),
            Some("This link shows paypal.com but opens evil.example.")
        );
        assert_eq!(link.target().as_ref(), "https://evil.example/login");
        assert_eq!(
            HoveredLink::resolve("https://other.example/", &links).warning(),
            None
        );
    }

    #[test]
    fn links_are_normalized_for_the_system() {
        let normalized = |href: &str| normalize_link(href).map(|url| url.to_string());
        for (href, expected) in [
            (
                " https://example.com/a b?q=c d ",
                "https://example.com/a%20b?q=c%20d",
            ),
            (
                "https://example.com/straße/ünïcode",
                "https://example.com/stra%C3%9Fe/%C3%BCn%C3%AFcode",
            ),
            (
                "https://example.com/a|b^c{d}?x=[1]",
                "https://example.com/a%7Cb%5Ec%7Bd%7D?x=%5B1%5D",
            ),
            (
                "https://Bücher.example/x",
                "https://xn--bcher-kva.example/x",
            ),
            (
                "https://example.com/a%20b?q=%7C#frag",
                "https://example.com/a%20b?q=%7C#frag",
            ),
            (
                "https://[2001:db8::1]:8080/p",
                "https://[2001:db8::1]:8080/p",
            ),
            ("www.example.com/x", "https://www.example.com/x"),
            ("mailto:anna@example.com", "mailto:anna@example.com"),
        ] {
            assert_eq!(normalized(href).as_deref(), Ok(expected), "{href}");
        }
        for href in ["not a link", "http://", "", "https://exa mple.com/"] {
            assert_eq!(
                normalized(href),
                Err(LinkError::Invalid(href.trim().to_owned())),
                "{href:?}"
            );
        }
    }

    #[test]
    fn only_web_and_mailto_links_open() {
        let click = |href: &str| link_click(href, &[]);
        assert!(matches!(click("http://example.com"), LinkClick::Open(_)));
        assert!(
            matches!(click("HTTPS://example.com/a b"), LinkClick::Open(url) if url.as_str() == "https://example.com/a%20b")
        );
        for (href, scheme) in [
            ("javascript:alert(1)", "javascript"),
            ("file:///etc/passwd", "file"),
            ("data:text/html,<b>x</b>", "data"),
            ("zoommtg://join?id=1", "zoommtg"),
            ("ftp://example.com/file", "ftp"),
        ] {
            assert_eq!(
                click(href),
                LinkClick::Refuse(LinkError::Scheme(scheme.to_owned())),
                "{href}"
            );
        }
        assert_eq!(
            LinkError::Scheme("javascript".into()).to_string(),
            "Maze doesn't open javascript links."
        );
        assert_eq!(
            click(
                "mailto:anna@example.com,ben%40example.org?cc=c@example.com&Subject=Hi%20there&body=Line%201"
            ),
            LinkClick::Compose(MailTo {
                to: vec!["anna@example.com".into(), "ben@example.org".into()],
                cc: vec!["c@example.com".into()],
                bcc: Vec::new(),
                subject: Some("Hi there".into()),
                body: Some("Line 1".into()),
            })
        );
        assert!(matches!(
            click("::"),
            LinkClick::Refuse(LinkError::Invalid(_))
        ));
    }

    #[test]
    fn deceptive_links_ask_first() {
        let links = [BodyLink {
            text: "www.paypal.com".into(),
            href: "https://login.evil.example/pay".into(),
            deception: Some(paypal()),
        }];
        let LinkClick::Confirm { url, opens, shown } =
            link_click(" https://login.evil.example/pay ", &links)
        else {
            panic!("a deceptive link must ask first");
        };
        assert_eq!(url.as_str(), "https://login.evil.example/pay");
        assert_eq!(opens, "login.evil.example");
        assert_eq!(shown.as_ref(), "paypal.com");
        // The same target behind honest text elsewhere doesn't make it less deceptive,
        // and other links open right away.
        assert!(matches!(
            link_click("https://example.com/", &links),
            LinkClick::Open(_)
        ));
    }

    #[test]
    fn detects_links_in_plain_text() {
        let text = "See https://example.com/a_(b), www.example.org. Mail mailto:anna@example.com!\n\
                    Not: https:// or www. or xhttps://no.example (https://in.example/x)";
        let links: Vec<_> = detect_links(text)
            .into_iter()
            .map(|(range, href)| (text[range].to_owned(), href.to_string()))
            .collect();
        assert_eq!(
            links,
            [
                (
                    "https://example.com/a_(b)".to_owned(),
                    "https://example.com/a_(b)".to_owned()
                ),
                (
                    "www.example.org".to_owned(),
                    "https://www.example.org".to_owned()
                ),
                (
                    "mailto:anna@example.com".to_owned(),
                    "mailto:anna@example.com".to_owned()
                ),
                (
                    "https://in.example/x".to_owned(),
                    "https://in.example/x".to_owned()
                ),
            ]
        );
        assert!(detect_links("äöü https://ü.example/ é").len() == 1);
    }
}

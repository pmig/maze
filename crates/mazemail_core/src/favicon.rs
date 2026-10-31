//! Favicons fetched from each domain's own website (PLAN-contacts §3b), never from a
//! third-party icon service.
//!
//! # Discovery
//!
//! 1. `https://<domain>/`, or `https://www.<domain>/` if the apex doesn't answer with
//!    a page. At most [`MAX_REDIRECTS`] redirects, HTTPS only, to host names only (no
//!    IP addresses); the first [`HTML_MAX_BYTES`] of the page are read.
//! 2. Its `<link rel="icon">`, `shortcut icon` and `apple-touch-icon` links (resolved
//!    against the page and its `<base href>`), best first: square sizes of at least
//!    64 px (the smallest of those), then 32–63 px, then undeclared sizes (an
//!    `apple-touch-icon` counts as 180 px), then smaller ones; PNG, ICO and SVG before
//!    other formats. Only icons on the domain itself (or the domain the page
//!    redirected to) are loaded, never a CDN elsewhere. At most [`MAX_CANDIDATES`].
//! 3. `/favicon.ico` of the page's host, then of the domain.
//!
//! An icon may weigh [`ICON_MAX_BYTES`]; its format is sniffed from the bytes (the
//! `Content-Type` is not trusted): PNG, ICO, JPEG, GIF and WebP are decoded with
//! `image` (dimension and memory limits), SVG is rendered with `resvg` (no scripts, no
//! external files or fonts). The result is a [`FAVICON_SIZE`]² PNG, stored in the
//! encrypted cache.
//!
//! # Privacy
//!
//! The HTTP client sends no cookies (none are kept), no `Referer`, no credentials and a
//! generic `User-Agent`; it connects in [`CONNECT_TIMEOUT`], gives each request
//! [`REQUEST_TIMEOUT`] and each domain [`DOMAIN_TIMEOUT`]. Host names that resolve to
//! private, loopback or link-local addresses are refused, and domains whose suffix is
//! not on the Public Suffix List (`.local`, `.internal`, `.example`) are never
//! fetched. Which domains are fetched at all is the `favicons` setting (see
//! [`FaviconService`]): `contacts` (default), `all` or `off`. Domain names are never
//! logged.
//!
//! # Scheduling
//!
//! [`FaviconService::run`] fetches one domain at a time with a pause between domains
//! ([`FETCH_INTERVAL`]): first the ones the UI asked for, then every other domain the
//! setting allows whose favicon is unknown or older than [`FAVICON_TTL`] (misses are
//! remembered as long), domains of contacts first. When domains stop answering in a
//! row it assumes Maze is offline and waits instead of recording misses.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use mazemail_mime::domain::{is_known_suffix, is_personal, registrable_domain};
use mazemail_mime::html::{IconRel, page_icons};
use settings::{FaviconPolicy, LoadedSettings};
use store::{
    DomainStats, FAVICON_CACHE_KIND, FAVICON_CACHE_VERSION, FaviconState, Queries as _, Store,
};
use tokio::sync::{Notify, watch};
use url::Url;

use crate::Result;

/// How long a fetched favicon (or a miss) is kept before the domain is asked again.
pub const FAVICON_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Width and height of a stored favicon (PNG).
pub const FAVICON_SIZE: u32 = 64;
/// How much of a domain's home page is read for its icon links.
pub const HTML_MAX_BYTES: usize = 512 * 1024;
/// The most an icon file may weigh.
pub const ICON_MAX_BYTES: usize = 256 * 1024;
/// Time to connect.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time for one request, in total.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Time for one domain, every request included.
pub const DOMAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Redirects followed per request.
pub const MAX_REDIRECTS: usize = 3;
/// Icon links tried per page.
pub const MAX_CANDIDATES: usize = 4;
/// Pause between two domains.
pub const FETCH_INTERVAL: Duration = Duration::from_secs(2);
/// How often the background sweep looks for domains that are due.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Unanswered domains in a row after which Maze is assumed offline.
const OFFLINE_AFTER: u32 = 3;
/// The wait after assuming Maze is offline.
const OFFLINE_BACKOFF: Duration = Duration::from_secs(10 * 60);
/// Decoded icons larger than this (either side) are refused.
const MAX_DECODED_SIDE: u32 = 2048;
/// Decoded icons smaller than this (either side) don't count.
const MIN_DECODED_SIDE: u32 = 16;
/// The User-Agent: the same for every Maze user, no version, no platform.
const USER_AGENT: &str = "Mozilla/5.0";

/// Where favicons are fetched from. [`FaviconConfig::internet`] in the app; the core
/// fetches nothing without one ([`CoreConfig::favicons`](crate::CoreConfig::favicons)).
#[derive(Debug, Clone)]
pub struct FaviconConfig {
    /// `https` (tests: `http`, against a local server).
    scheme: &'static str,
    /// A port added to every URL (tests only).
    port: Option<u16>,
    /// DNS answers (tests only).
    overrides: Vec<(String, SocketAddr)>,
    /// Refuse private addresses and unlisted suffixes (off in tests only).
    public_only: bool,
    /// Pause between two domains.
    interval: Duration,
}

impl FaviconConfig {
    /// HTTPS to each domain's own website.
    pub fn internet() -> Self {
        Self {
            scheme: "https",
            port: None,
            overrides: Vec::new(),
            public_only: true,
            interval: FETCH_INTERVAL,
        }
    }

    /// Plain HTTP to a local test server at `addr`, which answers for `hosts`.
    #[cfg(test)]
    pub(crate) fn local(addr: SocketAddr, hosts: &[&str]) -> Self {
        Self {
            scheme: "http",
            port: Some(addr.port()),
            overrides: hosts.iter().map(|h| ((*h).to_owned(), addr)).collect(),
            public_only: false,
            interval: Duration::ZERO,
        }
    }
}

/// Only public addresses: a domain's website must not be a way into the local network.
#[derive(Debug)]
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|addr| is_public(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("the host has no public address".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Whether an address is on the public internet.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || (a == 100 && (64..128).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 198 && (b == 18 || b == 19))
                || a >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80
                || (first == 0x2001 && v6.segments()[1] == 0x0db8))
        }
    }
}

/// What fetching a domain's favicon gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// A [`FAVICON_SIZE`]² PNG.
    Icon(Vec<u8>),
    /// The website answered, but has no usable icon.
    NoIcon,
    /// Neither the domain nor `www.` answered at all.
    Unreachable,
}

/// The HTTP side: finds, downloads and normalizes one domain's favicon.
pub(crate) struct FaviconFetcher {
    client: reqwest::Client,
    config: FaviconConfig,
}

impl std::fmt::Debug for FaviconFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaviconFetcher")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// A body read with a size cap.
struct Body {
    url: Url,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

impl FaviconFetcher {
    pub(crate) fn new(config: FaviconConfig) -> std::result::Result<Self, reqwest::Error> {
        let scheme = config.scheme;
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            // `previous` starts with the URL first asked for.
            if attempt.previous().len() > MAX_REDIRECTS {
                return attempt.error("too many redirects");
            }
            let url = attempt.url();
            if url.scheme() != scheme {
                return attempt.error("redirect to another scheme");
            }
            match url.host() {
                Some(url::Host::Domain(_)) => attempt.follow(),
                _ => attempt.error("redirect to an address"),
            }
        });
        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .referer(false)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .redirect(redirects)
            .https_only(config.scheme == "https");
        if config.public_only {
            builder = builder.dns_resolver(PublicOnlyResolver);
        }
        for (host, addr) in &config.overrides {
            builder = builder.resolve(host, *addr);
        }
        Ok(Self {
            client: builder.build()?,
            config,
        })
    }

    /// Whether this domain may be fetched at all: its suffix is a real one.
    pub(crate) fn may_fetch(&self, domain: &str) -> bool {
        !self.config.public_only || is_known_suffix(domain)
    }

    fn url(&self, host: &str, path: &str) -> Option<Url> {
        let port = self
            .config
            .port
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        Url::parse(&format!("{}://{host}{port}{path}", self.config.scheme)).ok()
    }

    /// Finds and normalizes `domain`'s favicon, within [`DOMAIN_TIMEOUT`].
    pub(crate) async fn fetch(&self, domain: &str) -> Fetched {
        match tokio::time::timeout(DOMAIN_TIMEOUT, self.discover(domain)).await {
            Ok(fetched) => fetched,
            // Something answered, slowly: not an outage.
            Err(_) => Fetched::NoIcon,
        }
    }

    async fn discover(&self, domain: &str) -> Fetched {
        let mut tried: HashSet<Url> = HashSet::new();
        let mut reached = false;
        let mut page_url: Option<Url> = None;
        for host in [domain.to_owned(), format!("www.{domain}")] {
            let Some(url) = self.url(&host, "/") else {
                continue;
            };
            match self
                .get(
                    &url,
                    "text/html,application/xhtml+xml",
                    HTML_MAX_BYTES,
                    true,
                )
                .await
            {
                Ok(Some(page)) => {
                    reached = true;
                    let html_like = page.content_type.as_deref().is_none_or(|t| {
                        t.starts_with("text/html") || t.starts_with("application/xhtml")
                    });
                    if html_like {
                        let html = String::from_utf8_lossy(&page.bytes);
                        let allowed = allowed_domains(domain, &page.url);
                        for candidate in candidates(&page.url, &html, self.config.scheme, &allowed)
                            .into_iter()
                            .take(MAX_CANDIDATES)
                        {
                            if tried.insert(candidate.clone())
                                && let Some(icon) = self.icon(&candidate).await
                            {
                                return Fetched::Icon(icon);
                            }
                        }
                    }
                    page_url = Some(page.url);
                    break;
                }
                // The server answered with an error status.
                Ok(None) => reached = true,
                Err(()) => {}
            }
        }
        let mut fallbacks = Vec::new();
        if let Some(page) = &page_url
            && let Ok(url) = page.join("/favicon.ico")
        {
            fallbacks.push(url);
        }
        fallbacks.extend(self.url(domain, "/favicon.ico"));
        for url in fallbacks {
            if tried.insert(url.clone())
                && let Some(icon) = self.icon(&url).await
            {
                return Fetched::Icon(icon);
            }
        }
        if reached {
            Fetched::NoIcon
        } else {
            Fetched::Unreachable
        }
    }

    /// Downloads and normalizes one icon.
    async fn icon(&self, url: &Url) -> Option<Vec<u8>> {
        let body = self
            .get(url, "image/*", ICON_MAX_BYTES, false)
            .await
            .ok()
            .flatten()?;
        normalize(&body.bytes, body.content_type.as_deref())
    }

    /// GET with a size cap: `Err` when nothing answered, `Ok(None)` for an error status,
    /// a redirect that isn't followed or (without `truncate`) a body over `max`.
    async fn get(
        &self,
        url: &Url,
        accept: &str,
        max: usize,
        truncate: bool,
    ) -> std::result::Result<Option<Body>, ()> {
        let mut response = match self
            .client
            .get(url.clone())
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
        {
            Ok(response) => response,
            // The server answered, with a redirect we don't follow.
            Err(err) if err.is_redirect() => return Ok(None),
            Err(err) => {
                tracing::debug!(timeout = err.is_timeout(), "favicon request failed");
                return Err(());
            }
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        if !truncate
            && response
                .content_length()
                .is_some_and(|len| len > max as u64)
        {
            return Ok(None);
        }
        let final_url = response.url().clone();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim().to_ascii_lowercase());
        let mut bytes = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(_) if truncate && !bytes.is_empty() => break,
                Err(_) => return Ok(None),
            };
            if bytes.len() + chunk.len() > max {
                if truncate {
                    bytes.extend_from_slice(&chunk[..max - bytes.len()]);
                    break;
                }
                return Ok(None);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Some(Body {
            url: final_url,
            content_type,
            bytes,
        }))
    }
}

/// The registrable domains icons may come from: the domain and the one its page
/// redirected to.
fn allowed_domains(domain: &str, page: &Url) -> Vec<String> {
    let mut allowed = vec![domain.to_owned()];
    if let Some(other) = page.host_str().and_then(registrable_domain)
        && other != domain
    {
        allowed.push(other);
    }
    allowed
}

/// The icon links of a page, resolved, filtered (scheme, host) and best first.
fn candidates(page: &Url, html: &str, scheme: &str, allowed: &[String]) -> Vec<Url> {
    let icons = page_icons(html);
    let base = icons
        .base
        .as_deref()
        .and_then(|base| page.join(base).ok())
        .unwrap_or_else(|| page.clone());
    let mut ranked: Vec<((u8, u8, u32), usize, Url)> = Vec::new();
    for (ix, link) in icons.links.iter().enumerate() {
        let Ok(url) = base.join(&link.href) else {
            continue;
        };
        if url.scheme() != scheme {
            continue;
        }
        let Some(url::Host::Domain(host)) = url.host() else {
            continue;
        };
        if !registrable_domain(host).is_some_and(|d| allowed.contains(&d)) {
            continue;
        }
        let path = url.path().to_ascii_lowercase();
        let svg = link.mime.as_deref() == Some("image/svg+xml") || path.ends_with(".svg");
        let preferred_format =
            svg || matches!(
                link.mime.as_deref(),
                Some("image/png" | "image/x-icon" | "image/vnd.microsoft.icon")
            ) || path.ends_with(".png")
                || path.ends_with(".ico");
        let size = link.size.or(match link.rel {
            IconRel::AppleTouchIcon => Some(180),
            IconRel::Icon => None,
        });
        let (class, distance) = match size {
            _ if svg && (link.any_size || link.size.is_none()) => (0, 0),
            Some(s) if s >= FAVICON_SIZE => (0, s - FAVICON_SIZE),
            Some(s) if s >= 32 => (1, FAVICON_SIZE - s),
            None => (2, 0),
            Some(s) => (3, FAVICON_SIZE - s),
        };
        ranked.push(((class, u8::from(!preferred_format), distance), ix, url));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut seen = HashSet::new();
    ranked
        .into_iter()
        .map(|(_, _, url)| url)
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

/// Whether the bytes are an SVG document.
fn looks_like_svg(bytes: &[u8], content_type: Option<&str>) -> bool {
    if content_type.is_some_and(|t| t.starts_with("image/svg+xml")) {
        return true;
    }
    let head = &bytes[..bytes.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg"))
}

/// Turns an icon file into a [`FAVICON_SIZE`]² PNG. `None` for anything that isn't a
/// usable image.
pub(crate) fn normalize(bytes: &[u8], content_type: Option<&str>) -> Option<Vec<u8>> {
    let image = if looks_like_svg(bytes, content_type) {
        render_svg(bytes)?
    } else {
        let format = image::guess_format(bytes).ok()?;
        if !matches!(
            format,
            image::ImageFormat::Png
                | image::ImageFormat::Ico
                | image::ImageFormat::Jpeg
                | image::ImageFormat::Gif
                | image::ImageFormat::WebP
        ) {
            return None;
        }
        let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(MAX_DECODED_SIDE);
        limits.max_image_height = Some(MAX_DECODED_SIDE);
        limits.max_alloc = Some(64 * 1024 * 1024);
        reader.limits(limits);
        let decoded = reader.decode().ok()?;
        if decoded.width() < MIN_DECODED_SIDE || decoded.height() < MIN_DECODED_SIDE {
            return None;
        }
        decoded
            .resize(
                FAVICON_SIZE,
                FAVICON_SIZE,
                image::imageops::FilterType::Lanczos3,
            )
            .to_rgba8()
    };
    let mut canvas = image::RgbaImage::new(FAVICON_SIZE, FAVICON_SIZE);
    let x = i64::from((FAVICON_SIZE - image.width().min(FAVICON_SIZE)) / 2);
    let y = i64::from((FAVICON_SIZE - image.height().min(FAVICON_SIZE)) / 2);
    image::imageops::overlay(&mut canvas, &image, x, y);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(canvas)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

/// Renders an SVG icon into a [`FAVICON_SIZE`]² bitmap: no scripts (resvg has none),
/// no external files, no nested SVG images.
fn render_svg(bytes: &[u8]) -> Option<image::RgbaImage> {
    use resvg::{tiny_skia, usvg};
    let default_data = usvg::ImageHrefResolver::default_data_resolver();
    let options = usvg::Options {
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(move |mime, data, options| {
                if mime.contains("svg") {
                    None
                } else {
                    default_data(mime, data, options)
                }
            }),
            resolve_string: Box::new(|_, _| None),
        },
        ..usvg::Options::default()
    };
    let tree = usvg::Tree::from_data(bytes, &options).ok()?;
    let size = tree.size();
    let longest = size.width().max(size.height());
    if !longest.is_finite() || longest <= 0.0 {
        return None;
    }
    let side = FAVICON_SIZE as f32;
    let scale = side / longest;
    let dx = (side - size.width() * scale) / 2.0;
    let dy = (side - size.height() * scale) / 2.0;
    let mut pixmap = tiny_skia::Pixmap::new(FAVICON_SIZE, FAVICON_SIZE)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
        &mut pixmap.as_mut(),
    );
    let mut image = image::RgbaImage::new(FAVICON_SIZE, FAVICON_SIZE);
    for (pixel, out) in pixmap.pixels().iter().zip(image.pixels_mut()) {
        let color = pixel.demultiply();
        *out = image::Rgba([color.red(), color.green(), color.blue(), color.alpha()]);
    }
    if image.pixels().all(|p| p.0[3] == 0) {
        return None;
    }
    Some(image)
}

/// Whether the `favicons` setting wants `domain`'s favicon: `contacts` for domains with
/// a contact or mail you sent there, `all` also for domains you only got mail from.
/// Free-mail domains are people, not organizations: never fetched.
pub(crate) fn wanted(
    policy: FaviconPolicy,
    domain: &str,
    stats: Option<&DomainStats>,
    contacts: u64,
) -> bool {
    if is_personal(domain) {
        return false;
    }
    let sent = stats.is_some_and(|s| s.sent_count > 0);
    let mail = stats.is_some_and(|s| s.message_count > 0);
    match policy {
        FaviconPolicy::Off => false,
        FaviconPolicy::Contacts => contacts > 0 || sent,
        FaviconPolicy::All => contacts > 0 || sent || mail,
    }
}

/// Whether a favicon (or a miss) is old enough to fetch again.
pub(crate) fn is_due(state: &FaviconState, now: i64) -> bool {
    state
        .checked()
        .is_none_or(|checked| now.saturating_sub(checked) >= FAVICON_TTL.as_secs() as i64)
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Domains the UI asked for, and the ones skipped in this run.
#[derive(Debug, Default)]
struct Queue {
    wanted: VecDeque<String>,
    queued: HashSet<String>,
    skipped: HashSet<String>,
}

/// The favicon side of the core: reads stored favicons, decides which domains to fetch
/// (the `favicons` setting, live) and fetches them in the background. See the [module
/// docs](self).
pub(crate) struct FaviconService {
    store: Store,
    fetcher: Option<FaviconFetcher>,
    interval: Duration,
    settings: watch::Receiver<Arc<LoadedSettings>>,
    queue: Mutex<Queue>,
    wake: Notify,
}

impl std::fmt::Debug for FaviconService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaviconService")
            .field("fetcher", &self.fetcher)
            .finish_non_exhaustive()
    }
}

impl FaviconService {
    /// A service over `store`; without `config` nothing is ever fetched.
    pub(crate) fn new(
        store: Store,
        config: Option<FaviconConfig>,
        settings: watch::Receiver<Arc<LoadedSettings>>,
    ) -> Self {
        let interval = config.as_ref().map_or(FETCH_INTERVAL, |c| c.interval);
        let fetcher = config.and_then(|config| {
            FaviconFetcher::new(config)
                .map_err(|err| tracing::warn!(%err, "no HTTP client for favicons"))
                .ok()
        });
        Self {
            store,
            fetcher,
            interval,
            settings,
            queue: Mutex::new(Queue::default()),
            wake: Notify::new(),
        }
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `favicons` setting right now.
    pub(crate) fn policy(&self) -> FaviconPolicy {
        self.settings.borrow().settings.favicons
    }

    /// The stored favicon of a registrable domain (a [`FAVICON_SIZE`]² PNG). Asks the
    /// background fetcher for it when it is unknown or due (the setting decides
    /// whether it is fetched). `None` with the setting `off`.
    pub(crate) fn favicon(&self, domain: &str) -> Result<Option<Vec<u8>>> {
        if self.policy() == FaviconPolicy::Off {
            return Ok(None);
        }
        let state = self
            .store
            .reader()?
            .domain_stat(domain)?
            .map_or(FaviconState::Unknown, |s| s.favicon);
        let mut icon = None;
        if let FaviconState::Stored { key, .. } = state {
            icon = self
                .store
                .blobs()
                .cache_get(&key, FAVICON_CACHE_KIND, FAVICON_CACHE_VERSION);
            if icon.is_none() {
                // The cache was cleared: fetch it again.
                let name = domain.to_owned();
                drop(self.store.write(move |w| w.reset_domain_favicon(&name)));
                self.request(domain);
                return Ok(None);
            }
        }
        if is_due(&state, unix_now()) {
            self.request(domain);
        }
        Ok(icon)
    }

    /// Puts a domain at the front of the background fetcher's queue (deduplicated).
    pub(crate) fn request(&self, domain: &str) {
        if self.fetcher.is_none() {
            return;
        }
        {
            let mut queue = self.queue();
            if !queue.queued.insert(domain.to_owned()) {
                return;
            }
            queue.wanted.push_back(domain.to_owned());
        }
        self.wake.notify_one();
    }

    /// The next domain to fetch: one the UI asked for (if the setting allows it and it
    /// is due), else the most relevant due domain the setting allows.
    fn next_due(&self, fetcher: &FaviconFetcher) -> Result<Option<String>> {
        let policy = self.policy();
        if policy == FaviconPolicy::Off {
            self.queue().wanted.clear();
            self.queue().queued.clear();
            return Ok(None);
        }
        let reader = self.store.reader()?;
        let stats: HashMap<String, DomainStats> = reader
            .domain_stats()?
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
        let contacts = reader.domain_contact_counts()?;
        let now = unix_now();
        let eligible = |domain: &str| {
            let stat = stats.get(domain);
            fetcher.may_fetch(domain)
                && wanted(
                    policy,
                    domain,
                    stat,
                    contacts.get(domain).copied().unwrap_or(0),
                )
                && stat.is_none_or(|s| is_due(&s.favicon, now))
        };
        loop {
            let next = {
                let mut queue = self.queue();
                let next = queue.wanted.pop_front();
                if let Some(domain) = &next {
                    queue.queued.remove(domain);
                }
                next
            };
            match next {
                Some(domain) if eligible(&domain) => return Ok(Some(domain)),
                Some(_) => continue,
                None => break,
            }
        }
        let skipped = self.queue().skipped.clone();
        let mut due: Vec<(u64, u64, u64, &str)> = stats
            .keys()
            .map(String::as_str)
            .chain(contacts.keys().map(String::as_str))
            .collect::<HashSet<&str>>()
            .into_iter()
            .filter(|domain| !skipped.contains(*domain) && eligible(domain))
            .map(|domain| {
                let stat = stats.get(domain);
                (
                    contacts.get(domain).copied().unwrap_or(0),
                    stat.map_or(0, |s| s.sent_count),
                    stat.map_or(0, |s| s.message_count),
                    domain,
                )
            })
            .collect();
        due.sort_by(|a, b| (b.0, b.1, b.2).cmp(&(a.0, a.1, a.2)).then(a.3.cmp(b.3)));
        Ok(due.first().map(|d| d.3.to_owned()))
    }

    /// Fetches the next due domain's favicon and stores the result. Returns the domain
    /// and what the fetch gave; `None` when nothing is due. A domain that doesn't answer
    /// at all is remembered as a miss only when Maze is `online` (the last domain did
    /// answer); otherwise it is skipped until the next sweep.
    pub(crate) async fn fetch_next(&self, online: bool) -> Result<Option<(String, Fetched)>> {
        let Some(fetcher) = &self.fetcher else {
            return Ok(None);
        };
        let Some(domain) = self.next_due(fetcher)? else {
            return Ok(None);
        };
        let fetched = fetcher.fetch(&domain).await;
        let name = domain.clone();
        match &fetched {
            Fetched::Icon(png) => {
                let png = png.clone();
                self.store
                    .write(move |w| w.store_domain_favicon(&name, &png))
                    .await?;
            }
            Fetched::Unreachable if !online => {
                self.queue().skipped.insert(domain.clone());
            }
            Fetched::NoIcon | Fetched::Unreachable => {
                self.store
                    .write(move |w| w.mark_domain_favicon_missing(&name))
                    .await?;
            }
        }
        Ok(Some((domain, fetched)))
    }

    /// The background loop: one domain at a time, [`FETCH_INTERVAL`] apart; sleeps
    /// until asked, the settings change or [`SWEEP_INTERVAL`] passes when nothing is
    /// due. Returns at once without a fetcher.
    pub(crate) async fn run(self: Arc<Self>) {
        if self.fetcher.is_none() {
            return;
        }
        let mut settings = self.settings.clone();
        let mut unanswered = 0u32;
        let mut online = false;
        let mut last_sweep = tokio::time::Instant::now();
        loop {
            if last_sweep.elapsed() >= SWEEP_INTERVAL {
                self.queue().skipped.clear();
                last_sweep = tokio::time::Instant::now();
            }
            let pause = match self.fetch_next(online).await {
                Ok(Some((_, fetched))) => {
                    online = fetched != Fetched::Unreachable;
                    if fetched == Fetched::Unreachable {
                        unanswered += 1;
                        if unanswered >= OFFLINE_AFTER {
                            // Probably offline: forget this run's skips and wait.
                            unanswered = 0;
                            self.queue().skipped.clear();
                            Some(OFFLINE_BACKOFF)
                        } else {
                            Some(self.interval)
                        }
                    } else {
                        unanswered = 0;
                        Some(self.interval)
                    }
                }
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(%err, "storing a favicon");
                    Some(OFFLINE_BACKOFF)
                }
            };
            match pause {
                Some(pause) => {
                    tokio::select! {
                        () = tokio::time::sleep(pause) => {}
                        changed = settings.changed() => if changed.is_err() { return },
                    }
                }
                None => {
                    tokio::select! {
                        () = self.wake.notified() => {}
                        changed = settings.changed() => if changed.is_err() { return },
                        () = tokio::time::sleep(SWEEP_INTERVAL) => {
                            self.queue().skipped.clear();
                            last_sweep = tokio::time::Instant::now();
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use mazemail_types::{Address, Flags, FolderRole};
    use pretty_assertions::assert_eq;
    use settings::Settings;
    use store::{AccountKind, NewAccount, NewFolder, NewLocation, NewMessage, StoreConfig};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;

    // ---- images -------------------------------------------------------------------

    fn png(side: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(side, side, image::Rgba([200, 30, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn ico(side: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(side, side, image::Rgba([0, 90, 200, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Ico)
            .unwrap();
        out
    }

    const SVG: &str = r#"<?xml version="1.0"?>
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
          <script>alert(1)</script>
          <image href="/etc/passwd" width="10" height="10"/>
          <rect width="10" height="10" fill="green"/>
        </svg>"#;

    fn decode(png: &[u8]) -> image::RgbaImage {
        image::load_from_memory_with_format(png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8()
    }

    #[test]
    fn icons_become_64_pixel_pngs() {
        for (bytes, content_type) in [
            (png(16), Some("image/png")),
            (png(180), None),
            (ico(32), Some("text/plain")),
            (SVG.as_bytes().to_vec(), Some("image/svg+xml")),
            (SVG.as_bytes().to_vec(), None),
        ] {
            let out = normalize(&bytes, content_type).unwrap();
            let image = decode(&out);
            assert_eq!(image.dimensions(), (FAVICON_SIZE, FAVICON_SIZE));
            assert_eq!(image.get_pixel(32, 32).0[3], 255, "{content_type:?}");
        }
        // A wide icon is centered on a transparent square.
        let wide = image::RgbaImage::from_pixel(64, 32, image::Rgba([1, 2, 3, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(wide)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let image = decode(&normalize(&bytes, None).unwrap());
        assert_eq!(image.get_pixel(32, 2).0[3], 0);
        assert_eq!(image.get_pixel(32, 32).0[3], 255);
    }

    #[test]
    fn non_images_are_refused() {
        assert_eq!(normalize(b"<!doctype html><title>404</title>", None), None);
        assert_eq!(normalize(b"<html>", Some("image/png")), None);
        assert_eq!(normalize(&png(8), None), None, "too small");
        assert_eq!(normalize(&[0x89, b'P', b'N', b'G', 0, 0], None), None);
        assert_eq!(
            normalize(b"<svg xmlns='http://www.w3.org/2000/svg'/>", None),
            None
        );
    }

    // ---- rules --------------------------------------------------------------------

    #[test]
    fn only_public_addresses() {
        for ip in [
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
            IpAddr::V6("2606:4700::1111".parse::<Ipv6Addr>().unwrap()),
        ] {
            assert!(is_public(ip), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "198.18.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.1",
            "2001:db8::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn candidates_are_resolved_filtered_and_ranked() {
        let page = Url::parse("https://www.northwind.example/en/").unwrap();
        let html = r#"
            <link rel="icon" href="favicon.ico">
            <link rel="icon" sizes="16x16" href="/16.png">
            <link rel="icon" sizes="32x32" href="/32.png">
            <link rel="icon" sizes="512x512" href="/512.png">
            <link rel="apple-touch-icon" href="https://static.northwind.example/apple.png">
            <link rel="icon" sizes="96x96" type="image/jpeg" href="/96.jpg">
            <link rel="icon" href="http://www.northwind.example/insecure.png">
            <link rel="icon" sizes="192x192" href="https://cdn.elsewhere.example/192.png">
            <link rel="icon" sizes="64x64" href="https://192.0.2.1/64.png">
        "#;
        let allowed = vec!["northwind.example".to_owned()];
        let urls: Vec<String> = candidates(&page, html, "https", &allowed)
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            urls,
            [
                "https://static.northwind.example/apple.png",
                "https://www.northwind.example/512.png",
                "https://www.northwind.example/96.jpg",
                "https://www.northwind.example/32.png",
                "https://www.northwind.example/en/favicon.ico",
                "https://www.northwind.example/16.png",
            ]
        );
        // `<base href>` changes where relative links point.
        let with_base = r#"<base href="/assets/"><link rel="icon" href="i.png">"#;
        let urls: Vec<String> = candidates(&page, with_base, "https", &allowed)
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(urls, ["https://www.northwind.example/assets/i.png"]);
    }

    #[test]
    fn eligibility_follows_the_setting() {
        let stats = |messages, sent| DomainStats {
            name: String::new(),
            message_count: messages,
            from_count: messages - sent,
            sent_count: sent,
            last_seen: None,
            favicon: FaviconState::Unknown,
        };
        let received_only = stats(5, 0);
        let sent_to = stats(5, 1);
        use FaviconPolicy::{All, Contacts, Off};
        assert!(wanted(Contacts, "n.example", None, 1));
        assert!(wanted(Contacts, "n.example", Some(&sent_to), 0));
        assert!(!wanted(Contacts, "n.example", Some(&received_only), 0));
        assert!(wanted(All, "n.example", Some(&received_only), 0));
        assert!(!wanted(Off, "n.example", Some(&sent_to), 3));
        assert!(!wanted(All, "gmail.com", Some(&sent_to), 3), "free mail");

        let day = 24 * 60 * 60;
        let now = 100 * day;
        assert!(is_due(&FaviconState::Unknown, now));
        assert!(!is_due(&FaviconState::Missing { checked: now - day }, now));
        assert!(is_due(
            &FaviconState::Missing {
                checked: now - 30 * day
            },
            now
        ));
        let key = mazemail_types::BlobId([0; 32]);
        assert!(!is_due(
            &FaviconState::Stored {
                key,
                checked: now - 29 * day
            },
            now
        ));
    }

    // ---- discovery against a local server -----------------------------------------

    /// Requests for `host` (any port).
    fn host(name: &'static str) -> impl Fn(&Request) -> bool + Send + Sync + 'static {
        move |req: &Request| {
            req.headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .and_then(|h| h.split(':').next())
                == Some(name)
        }
    }

    fn page(html: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_raw(html.as_bytes().to_vec(), "text/html; charset=utf-8")
    }

    fn image(bytes: Vec<u8>, content_type: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(bytes, content_type)
    }

    const HOSTS: &[&str] = &[
        "northwind.example",
        "www.northwind.example",
        "static.northwind.example",
        "cdn.elsewhere.example",
        "fabrikam.example",
        "www.fabrikam.example",
    ];

    fn fetcher(server: &MockServer) -> FaviconFetcher {
        FaviconFetcher::new(FaviconConfig::local(*server.address(), HOSTS)).unwrap()
    }

    fn icon_side(fetched: &Fetched) -> u32 {
        match fetched {
            Fetched::Icon(png) => decode(png).width(),
            other => panic!("no icon: {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_relative_icon_link_is_used() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(host("northwind.example"))
            .and(path("/"))
            .respond_with(page(
                r#"<html><head><link rel="icon" href="img/icon.png"></head>"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/img/icon.png"))
            .respond_with(image(png(32), "image/png"))
            .expect(1)
            .mount(&server)
            .await;
        let fetched = fetcher(&server).fetch("northwind.example").await;
        assert_eq!(icon_side(&fetched), FAVICON_SIZE);
    }

    #[tokio::test]
    async fn the_apple_touch_icon_wins_and_other_domains_are_never_asked() {
        let server = MockServer::start().await;
        let port = server.address().port();
        Mock::given(host("northwind.example"))
            .and(path("/"))
            .respond_with(page(&format!(
                r#"<link rel="icon" sizes="16x16" href="/16.png">
                   <link rel="icon" sizes="256x256" href="http://cdn.elsewhere.example:{port}/big.png">
                   <link rel="apple-touch-icon" href="http://static.northwind.example:{port}/apple.png">"#
            )))
            .mount(&server)
            .await;
        Mock::given(host("static.northwind.example"))
            .and(path("/apple.png"))
            .respond_with(image(png(180), "image/png"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/16.png"))
            .respond_with(image(png(16), "image/png"))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(host("cdn.elsewhere.example"))
            .respond_with(image(png(256), "image/png"))
            .expect(0)
            .mount(&server)
            .await;
        let fetched = fetcher(&server).fetch("northwind.example").await;
        assert_eq!(icon_side(&fetched), FAVICON_SIZE);
    }

    #[tokio::test]
    async fn without_links_favicon_ico_is_used_and_www_when_the_apex_fails() {
        let server = MockServer::start().await;
        Mock::given(host("northwind.example"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        Mock::given(host("www.northwind.example"))
            .and(path("/"))
            .respond_with(page("<html><head><title>Northwind</title></head></html>"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(host("www.northwind.example"))
            .and(path("/favicon.ico"))
            .respond_with(image(ico(32), "image/x-icon"))
            .expect(1)
            .mount(&server)
            .await;
        let fetched = fetcher(&server).fetch("northwind.example").await;
        assert_eq!(icon_side(&fetched), FAVICON_SIZE);
    }

    fn redirect(to: String) -> ResponseTemplate {
        ResponseTemplate::new(302).insert_header("location", to)
    }

    #[tokio::test]
    async fn at_most_three_redirects_are_followed() {
        let server = MockServer::start().await;
        let base = format!("http://northwind.example:{}", server.address().port());
        for (from, to) in [("/", "/r1"), ("/r1", "/r2"), ("/r2", "/r3")] {
            Mock::given(host("northwind.example"))
                .and(path(from))
                .respond_with(redirect(format!("{base}{to}")))
                .mount(&server)
                .await;
        }
        Mock::given(host("northwind.example"))
            .and(path("/r3"))
            .respond_with(page(r#"<link rel="icon" href="/i.png">"#))
            .mount(&server)
            .await;
        Mock::given(host("northwind.example"))
            .and(path("/i.png"))
            .respond_with(image(png(64), "image/png"))
            .mount(&server)
            .await;
        assert_eq!(
            icon_side(&fetcher(&server).fetch("northwind.example").await),
            FAVICON_SIZE
        );

        // Four in a row is one too many; so is a redirect to an IP address.
        let base = format!("http://fabrikam.example:{}", server.address().port());
        for (from, to) in [("/", "/a"), ("/a", "/b"), ("/b", "/c"), ("/c", "/d")] {
            Mock::given(host("fabrikam.example"))
                .and(path(from))
                .respond_with(redirect(format!("{base}{to}")))
                .mount(&server)
                .await;
        }
        Mock::given(host("fabrikam.example"))
            .and(path("/d"))
            .respond_with(page(r#"<link rel="icon" href="/i.png">"#))
            .expect(0)
            .mount(&server)
            .await;
        Mock::given(host("www.fabrikam.example"))
            .and(path("/"))
            .respond_with(redirect(format!(
                "http://127.0.0.1:{}/",
                server.address().port()
            )))
            .mount(&server)
            .await;
        Mock::given(host("fabrikam.example"))
            .and(path("/favicon.ico"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert_eq!(
            fetcher(&server).fetch("fabrikam.example").await,
            Fetched::NoIcon
        );
    }

    #[tokio::test]
    async fn oversized_and_non_image_icons_are_refused() {
        let server = MockServer::start().await;
        Mock::given(host("northwind.example"))
            .and(path("/"))
            .respond_with(page(
                r#"<link rel="icon" sizes="64x64" href="/huge.png">
                   <link rel="icon" sizes="48x48" href="/page.png">"#,
            ))
            .mount(&server)
            .await;
        let mut huge = png(64);
        huge.resize(ICON_MAX_BYTES + 1, 0);
        Mock::given(path("/huge.png"))
            .respond_with(image(huge, "image/png"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/page.png"))
            .respond_with(page("<html>not found</html>"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/favicon.ico"))
            .respond_with(page("<html>soft 404</html>"))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            fetcher(&server).fetch("northwind.example").await,
            Fetched::NoIcon
        );
    }

    #[tokio::test]
    async fn a_domain_nobody_answers_for_is_unreachable() {
        // A port nothing listens on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let fetcher = FaviconFetcher::new(FaviconConfig::local(
            addr,
            &["gone.example", "www.gone.example"],
        ))
        .unwrap();
        assert_eq!(fetcher.fetch("gone.example").await, Fetched::Unreachable);
    }

    #[test]
    fn production_is_https_only_to_listed_suffixes() {
        let fetcher = FaviconFetcher::new(FaviconConfig::internet()).unwrap();
        assert!(fetcher.may_fetch("northwind.com"));
        assert!(fetcher.may_fetch("bmi.gv.at"));
        assert!(!fetcher.may_fetch("northwind.example"));
        assert!(!fetcher.may_fetch("printer.local"));
        assert_eq!(
            fetcher
                .url("northwind.com", "/")
                .map(String::from)
                .as_deref(),
            Some("https://northwind.com/")
        );
        // A page's plain-HTTP icon links don't count.
        let page = Url::parse("https://northwind.com/").unwrap();
        let html = r#"<link rel="icon" href="http://northwind.com/i.png">"#;
        assert!(candidates(&page, html, "https", &["northwind.com".to_owned()]).is_empty());
    }

    // ---- the service --------------------------------------------------------------

    fn open_store(dir: &std::path::Path) -> Store {
        let mut config = StoreConfig::new(dir);
        config.gc_interval = None;
        let vault = vault::Vault::from_master_key(vault::MasterKey::from_bytes(
            vault::KeyId::FIRST,
            [5; vault::KEY_LEN],
        ));
        Store::open(&config, &vault).unwrap()
    }

    fn settings_with(policy: FaviconPolicy) -> Arc<LoadedSettings> {
        Arc::new(LoadedSettings {
            settings: Settings {
                favicons: policy,
                ..Settings::default()
            },
            diagnostics: Vec::new(),
        })
    }

    /// A contact at `northwind.example` (an address you wrote to), mail only received
    /// from `fabrikam.example`, and a contact at `gmail.com`.
    async fn populate(store: &Store) {
        store
            .write(|w| {
                w.bump_contact("anna@northwind.example", Some("Anna"), 1)?;
                w.upsert_contact("google:1", "carol@gmail.com", None, None)?;
                let account = w.create_account(&NewAccount {
                    kind: AccountKind::Imap,
                    name: "Work".to_owned(),
                    config_json: "{}".to_owned(),
                    ordinal: 0,
                })?;
                let inbox = w.create_folder(
                    &NewFolder::new(account, "INBOX", "Inbox").with_role(FolderRole::Inbox),
                )?;
                let mut m = NewMessage::new(account, 10);
                m.from = Some(Address::new(None::<String>, "news@fabrikam.example"));
                m.locations = vec![NewLocation::new(inbox, Some(1), Flags::empty())];
                w.upsert_message(&m)?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn serve_icons(server: &MockServer) {
        for name in ["northwind.example", "fabrikam.example"] {
            let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
            Mock::given(host(leaked))
                .and(path("/"))
                .respond_with(page(r#"<link rel="icon" href="/i.png">"#))
                .mount(server)
                .await;
        }
        Mock::given(path("/i.png"))
            .respond_with(image(png(32), "image/png"))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_setting_decides_which_domains_are_fetched() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        populate(&store).await;
        let server = MockServer::start().await;
        serve_icons(&server).await;
        let (settings, rx) = watch::channel(settings_with(FaviconPolicy::Off));
        let service = FaviconService::new(
            store.clone(),
            Some(FaviconConfig::local(*server.address(), HOSTS)),
            rx,
        );

        // Off: nothing, not even what the UI asks for.
        service.request("northwind.example");
        assert_eq!(service.fetch_next(true).await.unwrap(), None);
        assert_eq!(service.favicon("northwind.example").unwrap(), None);

        // Contacts: the contact's domain, never the free-mail one or a sender's.
        settings.send_replace(settings_with(FaviconPolicy::Contacts));
        let (domain, fetched) = service.fetch_next(true).await.unwrap().unwrap();
        assert_eq!(domain, "northwind.example");
        assert!(matches!(fetched, Fetched::Icon(_)));
        assert_eq!(service.fetch_next(true).await.unwrap(), None);
        service.request("fabrikam.example");
        assert_eq!(service.fetch_next(true).await.unwrap(), None);

        // The stored icon comes from the encrypted cache, without asking again.
        let requests = server.received_requests().await.unwrap().len();
        let icon = service.favicon("northwind.example").unwrap().unwrap();
        assert_eq!(decode(&icon).dimensions(), (FAVICON_SIZE, FAVICON_SIZE));
        assert_eq!(service.fetch_next(true).await.unwrap(), None);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);

        // All: also the domain you only got mail from.
        settings.send_replace(settings_with(FaviconPolicy::All));
        let (domain, _) = service.fetch_next(true).await.unwrap().unwrap();
        assert_eq!(domain, "fabrikam.example");
        assert_eq!(service.fetch_next(true).await.unwrap(), None);

        // Off again: stored icons aren't shown either.
        settings.send_replace(settings_with(FaviconPolicy::Off));
        assert_eq!(service.favicon("northwind.example").unwrap(), None);
    }

    #[tokio::test]
    async fn misses_are_remembered_and_unanswered_domains_skipped_while_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        populate(&store).await;
        let server = MockServer::start().await;
        Mock::given(host("northwind.example"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let (_settings, rx) = watch::channel(settings_with(FaviconPolicy::Contacts));
        let service = FaviconService::new(
            store.clone(),
            Some(FaviconConfig::local(*server.address(), HOSTS)),
            rx,
        );
        let (_, fetched) = service.fetch_next(true).await.unwrap().unwrap();
        assert_eq!(fetched, Fetched::NoIcon);
        let state = store
            .reader()
            .unwrap()
            .domain_stat("northwind.example")
            .unwrap()
            .unwrap()
            .favicon;
        assert!(matches!(state, FaviconState::Missing { .. }));
        // Not asked again until the TTL is over, even when the UI wants it.
        assert_eq!(service.favicon("northwind.example").unwrap(), None);
        assert_eq!(service.fetch_next(true).await.unwrap(), None);

        // Offline: nothing answers, nothing is recorded, the domain waits.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = listener.local_addr().unwrap();
        drop(listener);
        let (_settings, rx) = watch::channel(settings_with(FaviconPolicy::Contacts));
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        populate(&store).await;
        let offline =
            FaviconService::new(store.clone(), Some(FaviconConfig::local(closed, HOSTS)), rx);
        let (_, fetched) = offline.fetch_next(false).await.unwrap().unwrap();
        assert_eq!(fetched, Fetched::Unreachable);
        assert_eq!(
            store
                .reader()
                .unwrap()
                .domain_stat("northwind.example")
                .unwrap()
                .map(|s| s.favicon),
            None
        );
        assert_eq!(offline.fetch_next(false).await.unwrap(), None, "skipped");
    }
}

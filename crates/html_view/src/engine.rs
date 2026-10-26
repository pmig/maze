//! The Servo engine (feature `servo`): one [`Servo`] instance per process, created
//! the first time an original-mode view is shown, and one Servo `WebView` with its own
//! software rendering context per view.
//!
//! Servo runs on GPUI's main thread. Its event-loop waker (called from Servo's
//! threads) sends on a channel; a foreground task spins Servo's event loop and then
//! lets every live view pick up what its delegate collected (new frames, link clicks,
//! cursor and clipboard changes). Delegate callbacks never touch GPUI state directly,
//! because they run inside `spin_event_loop`.
//!
//! Hardening (ADR-020, docs/security.md):
//! - built without the `js_jit`, `multiprocess`, devtools, clipboard and web-API
//!   features; every JavaScript tier is switched off in the preferences;
//! - the document is served with `Content-Security-Policy: sandbox; default-src 'none'; …`,
//!   which disables scripting for it (Servo's `Document::scripting_enabled`);
//! - every load goes through [`WebViewDelegate::load_web_resource`] and is either
//!   answered from memory or cancelled; nothing reaches the network;
//! - temporary storage only (no config dir, no disk cache, no cookies kept).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, AsyncApp, Global, WeakEntity};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use servo::{
    AllowOrDenyRequest, ClipboardDelegate, Cursor, EventLoopWaker, NavigationRequest, Opts,
    Preferences, Servo, ServoBuilder, ServoDelegate, WebResourceLoad, WebResourceResponse, WebView,
    WebViewDelegate,
};
use url::Url;

use crate::document::OriginalDocument;
use crate::policy::{Navigation, Resource, navigation_for, resource_for};
use crate::view::{Fetch, OriginalView, Part, PartSource};

/// The process-wide engine.
pub(crate) struct Engine {
    servo: Servo,
    views: RefCell<Vec<WeakEntity<OriginalView>>>,
    wake: Wake,
}

/// Wakes the engine from any thread (a part fetched in the background is ready).
pub(crate) type Wake = Arc<dyn Fn() + Send + Sync>;

impl Global for Engine {}

#[derive(Clone)]
struct Waker(async_channel::Sender<()>);

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        // A full channel already has a wake-up pending.
        let _ = self.0.try_send(());
    }
}

/// Servo's preferences for mail: no script, no web APIs, nothing persistent.
pub(crate) fn preferences() -> Preferences {
    Preferences {
        js_disable_jit: true,
        js_baseline_interpreter_enabled: false,
        js_baseline_jit_enabled: false,
        js_ion_enabled: false,
        js_asmjs_enabled: false,
        js_wasm_enabled: false,
        js_wasm_baseline_enabled: false,
        js_wasm_ion_enabled: false,
        devtools_server_enabled: false,
        dom_serviceworker_enabled: false,
        dom_sharedworker_enabled: false,
        dom_worklet_enabled: false,
        dom_indexeddb_enabled: false,
        dom_cookiestore_enabled: false,
        dom_async_clipboard_enabled: false,
        dom_bluetooth_enabled: false,
        dom_geolocation_enabled: false,
        dom_gamepad_enabled: false,
        dom_notification_enabled: false,
        dom_permissions_enabled: false,
        dom_webgpu_enabled: false,
        dom_webxr_enabled: false,
        dom_webrtc_enabled: false,
        dom_navigator_protocol_handlers_enabled: false,
        dom_webgl2_enabled: false,
        network_http_cache_disabled: true,
        network_http_disk_cache: String::new(),
        session_history_max_length: 1,
        ..Preferences::default()
    }
}

fn opts() -> Opts {
    Opts {
        // A crash in the engine shows an error page instead of ending Maze.
        hard_fail: false,
        // No profile directory: cookies, storage and caches live in memory only.
        config_dir: None,
        temporary_storage: true,
        ..Opts::default()
    }
}

/// Servo with Maze's options, preferences and process-wide delegate.
pub(crate) fn build_servo(waker: Box<dyn EventLoopWaker>) -> Servo {
    let servo = ServoBuilder::default()
        .opts(opts())
        .preferences(preferences())
        .event_loop_waker(waker)
        .build();
    servo.set_delegate(Rc::new(EngineDelegate));
    servo
}

/// Loads that belong to no page (workers) are refused; devtools stay closed.
struct EngineDelegate;

impl ServoDelegate for EngineDelegate {
    fn load_web_resource(&self, load: WebResourceLoad) {
        refuse(load);
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        request.deny();
    }
}

impl Engine {
    /// The engine, started on first use.
    pub(crate) fn get(cx: &mut App) -> &Engine {
        if !cx.has_global::<Engine>() {
            let (tx, rx) = async_channel::bounded(1);
            let waker = Waker(tx);
            let servo = build_servo(Box::new(waker.clone()));
            cx.set_global(Engine {
                servo,
                views: RefCell::new(Vec::new()),
                wake: Arc::new(move || waker.wake()),
            });
            cx.spawn(async move |cx: &mut AsyncApp| {
                while rx.recv().await.is_ok() {
                    cx.update(Engine::spin);
                }
            })
            .detach();
        }
        cx.global::<Engine>()
    }

    pub(crate) fn servo(&self) -> &Servo {
        &self.servo
    }

    pub(crate) fn wake(&self) -> Wake {
        self.wake.clone()
    }

    pub(crate) fn register(&self, view: WeakEntity<OriginalView>) {
        self.views.borrow_mut().push(view);
    }

    /// Spins Servo once and hands each view what its delegate collected.
    pub(crate) fn spin(cx: &mut App) {
        let views = {
            let Some(engine) = cx.try_global::<Engine>() else {
                return;
            };
            engine.servo.spin_event_loop();
            let mut views = engine.views.borrow_mut();
            views.retain(|view| view.upgrade().is_some());
            views.clone()
        };
        for view in views {
            let _ = view.update(cx, |view, cx| view.after_spin(cx));
        }
    }
}

/// What a view's delegate collected during a spin.
#[derive(Default)]
pub(crate) struct Inbox {
    pub(crate) frame_ready: Cell<bool>,
    pub(crate) cursor: Cell<Option<Cursor>>,
    pub(crate) open: RefCell<Vec<Url>>,
    pub(crate) clipboard: RefCell<Option<String>>,
    /// The document generation whose load completed last.
    pub(crate) loaded: Cell<Option<u64>>,
    /// The status text changed: the link under the pointer (`None`: none).
    pub(crate) status: RefCell<Option<Option<String>>>,
}

/// The document a view shows now, with where its parts come from.
pub(crate) struct Current {
    pub(crate) document: OriginalDocument,
    pub(crate) parts: Rc<dyn PartSource>,
}

/// The delegate of one view: answers loads from memory and decides navigation. A view
/// reuses its page for the next message: [`Delegate::current`] changes, and loads that
/// belong to an earlier document are refused.
pub(crate) struct Delegate {
    pub(crate) current: RefCell<Current>,
    pub(crate) inbox: Rc<Inbox>,
    /// Loads waiting for a part fetched in the background.
    pending: RefCell<Vec<Pending>>,
    wake: Wake,
}

/// A load waiting for its part.
struct Pending {
    generation: u64,
    load: WebResourceLoad,
    part: std::sync::mpsc::Receiver<Option<Part>>,
}

impl Delegate {
    pub(crate) fn new(
        document: OriginalDocument,
        parts: Rc<dyn PartSource>,
        inbox: Rc<Inbox>,
        wake: Wake,
    ) -> Self {
        Self {
            current: RefCell::new(Current { document, parts }),
            inbox,
            pending: RefCell::new(Vec::new()),
            wake,
        }
    }

    fn generation(&self) -> u64 {
        self.current.borrow().document.generation()
    }

    /// Fetches a part off the main thread (a blob read, or a remote image Maze
    /// downloads) and answers `load` once it's there ([`Delegate::poll_pending`]).
    fn fetch(&self, generation: u64, load: WebResourceLoad, fetch: Option<Fetch>) {
        let Some(fetch) = fetch else {
            return refuse(load);
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = self.wake.clone();
        let spawned = std::thread::Builder::new()
            .name("original-html-part".into())
            .spawn(move || {
                let _ = tx.send(fetch());
                wake();
            });
        match spawned {
            Ok(_) => self.pending.borrow_mut().push(Pending {
                generation,
                load,
                part: rx,
            }),
            Err(_) => refuse(load),
        }
    }

    /// Answers the loads whose parts arrived; refuses those of an earlier document.
    pub(crate) fn poll_pending(&self) {
        let current = self.generation();
        let mut pending = self.pending.borrow_mut();
        let mut waiting = Vec::with_capacity(pending.len());
        for entry in pending.drain(..) {
            if entry.generation != current {
                refuse(entry.load);
                continue;
            }
            match entry.part.try_recv() {
                Ok(Some(part)) if part.content_type.starts_with("image/") => {
                    respond(entry.load, &part.content_type, None, part.bytes);
                }
                Ok(_) | Err(std::sync::mpsc::TryRecvError::Disconnected) => refuse(entry.load),
                Err(std::sync::mpsc::TryRecvError::Empty) => waiting.push(entry),
            }
        }
        *pending = waiting;
    }
}

fn respond(load: WebResourceLoad, content_type: &str, csp: Option<&str>, body: Vec<u8>) {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Some(csp) = csp.and_then(|csp| HeaderValue::from_str(csp).ok()) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    let url = load.request().url.clone();
    let mut intercepted = load.intercept(
        WebResourceResponse::new(url)
            .headers(headers)
            .status_code(StatusCode::OK),
    );
    intercepted.send_body_data(body);
    intercepted.finish();
}

/// Answers with an empty 404 (a cancelled load can leave the page waiting for it).
fn refuse(load: WebResourceLoad) {
    let url = load.request().url.clone();
    let mut headers = HeaderMap::new();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    load.intercept(
        WebResourceResponse::new(url)
            .headers(headers)
            .status_code(StatusCode::NOT_FOUND),
    )
    .finish();
}

impl WebViewDelegate for Delegate {
    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        let url = load.request().url.clone();
        let current = self.current.borrow();
        let document = &current.document;
        let located = resource_for(&url).filter(|l| l.generation == document.generation());
        let Some(located) = located else {
            return refuse(load);
        };
        let generation = located.generation;
        match located.resource {
            Resource::Document => respond(
                load,
                "text/html; charset=utf-8",
                Some(document.content_security_policy()),
                document.html().as_bytes().to_vec(),
            ),
            Resource::Cid(id) => {
                let fetch = current.parts.inline_part(&id);
                drop(current);
                self.fetch(generation, load, fetch);
            }
            Resource::Remote(n) => {
                let fetch = document
                    .remote_url(n)
                    .and_then(|remote| current.parts.remote(remote));
                drop(current);
                self.fetch(generation, load, fetch);
            }
        }
    }

    fn request_navigation(&self, _webview: WebView, request: NavigationRequest) {
        match navigation_for(&request.url, self.generation()) {
            Navigation::Load => request.allow(),
            Navigation::OpenExternally(url) => {
                self.inbox.open.borrow_mut().push(url);
                request.deny();
            }
            Navigation::Deny => request.deny(),
        }
    }

    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.inbox.frame_ready.set(true);
    }

    fn notify_status_text_changed(&self, _webview: WebView, status: Option<String>) {
        *self.inbox.status.borrow_mut() = Some(status.filter(|text| !text.is_empty()));
    }

    fn notify_cursor_changed(&self, _webview: WebView, cursor: Cursor) {
        self.inbox.cursor.set(Some(cursor));
    }

    fn notify_load_status_changed(&self, webview: WebView, status: servo::LoadStatus) {
        if status == servo::LoadStatus::Complete {
            let generation = self.generation();
            if webview.url().as_ref() == Some(&crate::document::document_url(generation)) {
                self.inbox.loaded.set(Some(generation));
            }
        }
    }
}

/// Copy from the page goes to the GPUI clipboard; the page can't read it.
pub(crate) struct Clipboard {
    pub(crate) inbox: Rc<Inbox>,
}

impl ClipboardDelegate for Clipboard {
    fn set_text(&self, _webview: WebView, new_contents: String) {
        *self.inbox.clipboard.borrow_mut() = Some(new_contents);
    }

    fn get_text(&self, _webview: WebView, request: servo::StringRequest) {
        request.failure("Reading the clipboard is not allowed".into());
    }
}

#[cfg(test)]
mod tests {
    //! Servo itself, headless: the hardening must hold in the real engine. One test
    //! (one Servo per process).

    use std::io::Read as _;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use servo::{
        DevicePoint, InputEvent, MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent,
        RenderingContext as _, SoftwareRenderingContext, WebViewBuilder,
    };

    use super::*;
    use crate::document::{DocumentOptions, document_url};
    use crate::view::Part;

    struct Parts;

    impl PartSource for Parts {
        fn inline_part(&self, content_id: &str) -> Option<Fetch> {
            (content_id == "dot").then(|| -> Fetch {
                Box::new(|| {
                    Some(Part {
                        content_type: "image/png".into(),
                        bytes: blue_png(),
                    })
                })
            })
        }

        fn remote(&self, _url: &str) -> Option<Fetch> {
            panic!("remote content is not allowed for this message");
        }
    }

    fn no_wake() -> Wake {
        Arc::new(|| {})
    }

    /// A 1×1 blue PNG.
    fn blue_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 255, 255]));
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[derive(Clone)]
    struct Flag(Arc<AtomicBool>);

    impl EventLoopWaker for Flag {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }

        fn wake(&self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    fn spin_until(
        servo: &Servo,
        delegate: &Delegate,
        timeout: Duration,
        mut done: impl FnMut() -> bool,
    ) -> bool {
        let start = Instant::now();
        while !done() {
            if start.elapsed() > timeout {
                return false;
            }
            servo.spin_event_loop();
            delegate.poll_pending();
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    #[test]
    fn servo_renders_the_message_and_nothing_else() {
        // Anything that reaches the network would hit this listener.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();

        // Not sanitized on purpose: the engine layer must hold on its own.
        let html = format!(
            r#"<style>body {{ margin: 0; background: rgb(255, 0, 0); }}</style>
            <script>document.body.style.background = "rgb(0, 255, 0)";</script>
            <body onload="document.body.style.background = 'rgb(0, 255, 0)'">
            <img src="cid:dot" width="40" height="40" style="display:block">
            <img src="http://127.0.0.1:{port}/pixel.png" width="1" height="1">
            <link rel="stylesheet" href="http://127.0.0.1:{port}/style.css">
            <a id="link" href="https://shop.example/deal"
               style="display:block;width:200px;height:100px;background:rgb(255,0,0)">deal</a>
            <iframe src="http://127.0.0.1:{port}/frame"></iframe>"#
        );
        let document = OriginalDocument::build(&html, &DocumentOptions::default(), 1);

        let size = dpi::PhysicalSize::new(400, 300);
        let context = Rc::new(SoftwareRenderingContext::new(size).unwrap());
        context.make_current().unwrap();
        let started = Instant::now();
        let servo = build_servo(Box::new(Flag(Arc::new(AtomicBool::new(false)))));
        let inbox = Rc::new(Inbox::default());
        let delegate = Rc::new(Delegate::new(
            document,
            Rc::new(Parts),
            inbox.clone(),
            no_wake(),
        ));
        let webview = WebViewBuilder::new(&servo, context.clone())
            .delegate(delegate.clone())
            .url(document_url(1))
            .build();

        assert!(
            spin_until(&servo, &delegate, Duration::from_secs(30), || inbox
                .loaded
                .get()
                == Some(1)
                && inbox.frame_ready.get()),
            "the message never finished loading"
        );
        let rect = servo::DeviceIntRect::from_size(servo::DeviceIntSize::new(400, 300));
        let first_frame = started.elapsed();
        // Images decode asynchronously: paint frames until the inline part shows.
        let mut image = None;
        spin_until(&servo, &delegate, Duration::from_secs(10), || {
            if inbox.frame_ready.replace(false) {
                webview.paint();
                image = context.read_to_image(rect);
            }
            image
                .as_ref()
                .is_some_and(|image| image.get_pixel(20, 20).0 == [0, 0, 255, 255])
        });
        let image = image.unwrap();
        // The inline part was served from memory.
        assert_eq!(image.get_pixel(20, 20).0, [0, 0, 255, 255]);
        // The page background stayed red: neither the script nor `onload` ran.
        assert_eq!(image.get_pixel(300, 250).0, [255, 0, 0, 255]);
        // Nothing connected to the outside.
        std::thread::sleep(Duration::from_millis(200));
        let mut buf = [0; 16];
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.read(&mut buf);
                panic!("the engine made a network request");
            }
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
        }

        // A click on the link is reported, and the page stays.
        let point: servo::WebViewPoint = DevicePoint::new(100., 90.).into();
        webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point)));
        for action in [MouseButtonAction::Down, MouseButtonAction::Up] {
            webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                action,
                MouseButton::Primary,
                point,
            )));
        }
        assert!(
            spin_until(&servo, &delegate, Duration::from_secs(10), || !inbox
                .open
                .borrow()
                .is_empty()),
            "the click wasn't reported"
        );
        assert_eq!(
            inbox.open.borrow().as_slice(),
            &[Url::parse("https://shop.example/deal").unwrap()]
        );
        assert_eq!(webview.url(), Some(document_url(1)));
        eprintln!("first frame after {first_frame:?}");

        // The next message reuses the page: a new document number, and loads of the
        // old one are refused.
        let next = OriginalDocument::build(
            "<style>body { margin: 0; background: rgb(0, 0, 255); }</style><p>Next</p>",
            &DocumentOptions::default(),
            2,
        );
        let switched = Instant::now();
        delegate.current.borrow_mut().document = next;
        inbox.frame_ready.set(false);
        webview.load(document_url(2));
        assert!(
            spin_until(&servo, &delegate, Duration::from_secs(30), || inbox
                .loaded
                .get()
                == Some(2)
                && inbox.frame_ready.get()),
            "the next message never loaded"
        );
        webview.paint();
        let switch = switched.elapsed();
        let image = context.read_to_image(rect).unwrap();
        assert_eq!(image.get_pixel(300, 250).0, [0, 0, 255, 255]);
        eprintln!("next message after {switch:?}");
        drop(webview);

        // The `sandbox` header alone already stops scripts (the CSP's `default-src`
        // does too; each layer holds without the other).
        let script = r#"<style>body { margin: 0; background: rgb(255, 0, 0); }</style>
            <script>document.body.style.background = "rgb(0, 255, 0)";</script>"#;
        let inbox = Rc::new(Inbox::default());
        let delegate = Rc::new(Delegate::new(
            OriginalDocument::with_header_only(script, "sandbox"),
            Rc::new(Parts),
            inbox.clone(),
            no_wake(),
        ));
        let webview = WebViewBuilder::new(&servo, context.clone())
            .delegate(delegate.clone())
            .url(document_url(0))
            .build();
        assert!(spin_until(
            &servo,
            &delegate,
            Duration::from_secs(30),
            || inbox.loaded.get() == Some(0) && inbox.frame_ready.get()
        ));
        webview.paint();
        let image = context.read_to_image(rect).unwrap();
        assert_eq!(image.get_pixel(300, 250).0, [255, 0, 0, 255]);
    }
}

#[cfg(test)]
mod measure {
    //! `cargo test -p html_view --features servo -- --ignored --nocapture measure`:
    //! resident memory and first-frame times for a few views (not run by default).

    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use servo::{RenderingContext as _, SoftwareRenderingContext, WebViewBuilder};

    use super::*;
    use crate::document::{DocumentOptions, document_url};
    use crate::view::Fetch;

    struct NoParts;

    impl PartSource for NoParts {
        fn inline_part(&self, _: &str) -> Option<Fetch> {
            None
        }

        fn remote(&self, _: &str) -> Option<Fetch> {
            None
        }
    }

    fn rss_mb() -> f64 {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<f64>()
            .unwrap()
            / 1024.
    }

    /// A newsletter-like page: nested tables, styles, 200 paragraphs.
    fn newsletter() -> String {
        let mut html = String::from(
            "<style>td{font:15px Helvetica,Arial;color:#333}h1{color:#c00}.btn{background:#06c;color:#fff;padding:8px 16px;border-radius:4px}</style>\
             <table width=\"100%\" bgcolor=\"#eeeeee\"><tr><td align=\"center\"><table width=\"600\" bgcolor=\"#ffffff\">",
        );
        for i in 0..200 {
            html.push_str(&format!(
                "<tr><td><h1>Story {i}</h1><p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor.</p><a class=\"btn\" href=\"https://shop.example/{i}\">Read</a></td></tr>"
            ));
        }
        html.push_str("</table></td></tr></table>");
        html
    }

    #[test]
    #[ignore = "a measurement, not a check"]
    fn measure_memory_and_first_frame() {
        let base = rss_mb();
        let servo = build_servo(Box::new(NoWake));
        let wake: Wake = Arc::new(|| {});
        let mut views = Vec::new();
        for n in 1..=3u64 {
            let size = dpi::PhysicalSize::new(1600, 1800);
            let context = Rc::new(SoftwareRenderingContext::new(size).unwrap());
            let inbox = Rc::new(Inbox::default());
            let document = OriginalDocument::build(&newsletter(), &DocumentOptions::default(), n);
            let delegate = Rc::new(Delegate::new(
                document,
                Rc::new(NoParts),
                inbox.clone(),
                wake.clone(),
            ));
            let started = Instant::now();
            let webview = WebViewBuilder::new(&servo, context.clone())
                .delegate(delegate.clone())
                .url(document_url(n))
                .build();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !(inbox.loaded.get() == Some(n) && inbox.frame_ready.get()) {
                assert!(Instant::now() < deadline);
                servo.spin_event_loop();
                std::thread::sleep(Duration::from_millis(1));
            }
            webview.paint();
            let rect = servo::DeviceIntRect::from_size(servo::DeviceIntSize::new(1600, 1800));
            let _ = context.read_to_image(rect).unwrap();
            eprintln!(
                "view {n}: first frame {:?}, rss {:.0} MB (+{:.0} MB since start)",
                started.elapsed(),
                rss_mb(),
                rss_mb() - base
            );
            views.push((webview, context, delegate));
        }
    }

    #[derive(Clone)]
    struct NoWake;

    impl EventLoopWaker for NoWake {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }

        fn wake(&self) {}
    }
}

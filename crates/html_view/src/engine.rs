//! The Blitz engine (feature `blitz`): one Blitz document per message and view,
//! rendered on the CPU into an image.
//!
//! Blitz parses and lays out HTML and CSS (Stylo, Taffy, Parley) and has no JavaScript
//! engine at all, so a message can't run a script whatever its HTML says. A [`Page`]
//! owns the document of the message a view shows; it is driven on GPUI's main thread.
//! Blitz reports resource loads, link clicks, cursor changes and redraw requests
//! through its providers, which may be called from other threads, so the providers
//! only queue them and wake the engine; a foreground task then lets every live view
//! poll its page ([`Page::poll`]).
//!
//! Hardening (ADR-033, docs/security.md):
//! - no script engine, no networking: Blitz is built without `blitz-net`; every
//!   load goes through [`Providers`] and is either answered from memory or refused;
//! - only parts of the current document are served (`policy::resource_for` and the
//!   document number), images only;
//! - link clicks are reported, never followed (`policy::navigation_for`); forms
//!   never submit.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyrender::ImageRenderer as _;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{BaseDocument, DocumentConfig, EventDriver, NoopEventHandler, StyleThreading};
use blitz_html::HtmlDocument;
use blitz_traits::events::{
    BlitzPointerEvent, BlitzPointerId, BlitzWheelDelta, BlitzWheelEvent, MouseEventButton,
    MouseEventButtons, PointerCoords, PointerDetails, UiEvent,
};
use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::{Body, Bytes, Method, NetHandler, NetProvider, Request};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
use cursor_icon::CursorIcon;
use gpui::{App, AsyncApp, Global, WeakEntity};
use url::Url;

use crate::document::OriginalDocument;
use crate::policy::{Navigation, Resource, navigation_for, resource_for};
use crate::view::{Fetch, OriginalView, PartSource};

/// The process-wide engine: wakes the views when one of their pages has news.
pub(crate) struct Engine {
    views: RefCell<Vec<WeakEntity<OriginalView>>>,
    wake: Wake,
}

/// Wakes the engine from any thread (a part fetched in the background is ready).
pub(crate) type Wake = Arc<dyn Fn() + Send + Sync>;

impl Global for Engine {}

impl Engine {
    /// The engine, started on first use.
    pub(crate) fn get(cx: &mut App) -> &Engine {
        if !cx.has_global::<Engine>() {
            let (tx, rx) = async_channel::bounded::<()>(1);
            cx.set_global(Engine {
                views: RefCell::new(Vec::new()),
                // A full channel already has a wake-up pending.
                wake: Arc::new(move || {
                    let _ = tx.try_send(());
                }),
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

    pub(crate) fn wake(&self) -> Wake {
        self.wake.clone()
    }

    pub(crate) fn register(&self, view: WeakEntity<OriginalView>) {
        self.views.borrow_mut().push(view);
    }

    /// Lets each view pick up what its page collected.
    pub(crate) fn spin(cx: &mut App) {
        let views = {
            let Some(engine) = cx.try_global::<Engine>() else {
                return;
            };
            let mut views = engine.views.borrow_mut();
            views.retain(|view| view.upgrade().is_some());
            views.clone()
        };
        for view in views {
            let _ = view.update(cx, |view, cx| view.after_spin(cx));
        }
    }
}

/// What a page collected since the view last looked.
#[derive(Default)]
pub(crate) struct Inbox {
    pub(crate) frame_ready: Cell<bool>,
    pub(crate) cursor: Cell<Option<CursorIcon>>,
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

/// Blitz's providers for one document: they only queue what Blitz asks for (from any
/// thread) and wake the engine; [`Page::poll`] decides on the main thread.
struct Providers {
    generation: u64,
    requests: Mutex<Vec<(Request, Box<dyn NetHandler>)>>,
    navigations: Mutex<Vec<NavigationOptions>>,
    cursor: Mutex<Option<CursorIcon>>,
    cursor_changed: AtomicBool,
    redraw: AtomicBool,
    wake: Wake,
}

impl Providers {
    fn new(generation: u64, wake: Wake) -> Self {
        Self {
            generation,
            requests: Mutex::new(Vec::new()),
            navigations: Mutex::new(Vec::new()),
            cursor: Mutex::new(None),
            cursor_changed: AtomicBool::new(false),
            redraw: AtomicBool::new(false),
            wake,
        }
    }
}

impl NetProvider for Providers {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((request, handler));
        (self.wake)();
    }
}

impl NavigationProvider for Providers {
    fn navigate_to(&self, options: NavigationOptions) {
        self.navigations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(options);
        (self.wake)();
    }
}

impl ShellProvider for Providers {
    fn request_redraw(&self) {
        self.redraw.store(true, Ordering::Release);
        (self.wake)();
    }

    fn set_cursor(&self, icon: Option<CursorIcon>) {
        *self.cursor.lock().unwrap_or_else(PoisonError::into_inner) = icon;
        self.cursor_changed.store(true, Ordering::Release);
    }
}

/// Answers a load with nothing: an image without bytes fails to decode, a stylesheet
/// is empty (and stops blocking the first paint).
fn refuse(handler: Box<dyn NetHandler>, url: &Url) {
    handler.bytes(url.to_string(), Bytes::new());
}

/// The page of one view: the current document and the Blitz document of the one it
/// loaded last.
pub(crate) struct Page {
    current: RefCell<Current>,
    inbox: Rc<Inbox>,
    wake: Wake,
    loaded: RefCell<Option<Loaded>>,
    viewport: RefCell<Viewport>,
    renderer: RefCell<Option<(VelloCpuImageRenderer, (u32, u32))>>,
    started: Instant,
}

/// A document Blitz has.
struct Loaded {
    document: HtmlDocument,
    providers: Arc<Providers>,
    /// The pointer's primary button is down (a drag selects text).
    pressed: Cell<bool>,
    hover: Cell<Option<blitz_dom::NodeId>>,
}

impl Page {
    /// A page for `document` at `(width, height)` device pixels and `scale`; it loads
    /// with [`Page::load`].
    pub(crate) fn new(
        document: OriginalDocument,
        parts: Rc<dyn PartSource>,
        inbox: Rc<Inbox>,
        wake: Wake,
        (width, height, scale): (u32, u32, f32),
    ) -> Self {
        Self {
            current: RefCell::new(Current { document, parts }),
            inbox,
            wake,
            loaded: RefCell::new(None),
            viewport: RefCell::new(Viewport::new(width, height, scale, ColorScheme::Light)),
            renderer: RefCell::new(None),
            started: Instant::now(),
        }
    }

    /// Makes `document` the current one at once (so loads of the previous one are
    /// refused from now on); the page shows it after [`Page::load`].
    pub(crate) fn switch_to(&self, document: OriginalDocument, parts: Rc<dyn PartSource>) {
        *self.current.borrow_mut() = Current { document, parts };
    }

    pub(crate) fn generation(&self) -> u64 {
        self.current.borrow().document.generation()
    }

    /// The URL of the document the page shows (or loads).
    #[cfg(test)]
    pub(crate) fn url(&self) -> Option<Url> {
        self.loaded
            .borrow()
            .as_ref()
            .map(|loaded| crate::document::document_url(loaded.providers.generation))
    }

    /// Parses the current document into a new Blitz document. Its parts load through
    /// [`Page::poll`].
    pub(crate) fn load(&self) {
        let (generation, html, url) = {
            let current = self.current.borrow();
            (
                current.document.generation(),
                current.document.html().to_owned(),
                current.document.url(),
            )
        };
        let providers = Arc::new(Providers::new(generation, self.wake.clone()));
        let config = DocumentConfig {
            viewport: Some(self.viewport.borrow().clone()),
            base_url: Some(url.to_string()),
            net_provider: Some(providers.clone()),
            navigation_provider: Some(providers.clone()),
            shell_provider: Some(providers.clone()),
            style_threading: StyleThreading::Sequential,
            ..DocumentConfig::default()
        };
        let document = HtmlDocument::from_html(&html, config);
        *self.loaded.borrow_mut() = Some(Loaded {
            document,
            providers,
            pressed: Cell::new(false),
            hover: Cell::new(None),
        });
        self.poll();
    }

    /// Answers queued loads, reports link clicks, cursor changes and new frames, and
    /// lays the document out. Marks the document loaded once nothing blocks its first
    /// paint.
    pub(crate) fn poll(&self) {
        let mut loaded = self.loaded.borrow_mut();
        let Some(loaded) = loaded.as_mut() else {
            return;
        };
        let providers = loaded.providers.clone();
        self.answer(&providers);
        loaded
            .document
            .resolve(self.started.elapsed().as_secs_f64());
        // Resolving can ask for images it found in styles.
        self.answer(&providers);

        let navigations = std::mem::take(
            &mut *providers
                .navigations
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for navigation in navigations {
            self.navigate(providers.generation, &navigation);
        }
        if providers.cursor_changed.swap(false, Ordering::AcqRel) {
            let cursor = *providers
                .cursor
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            self.inbox
                .cursor
                .set(Some(cursor.unwrap_or(CursorIcon::Default)));
        }
        if providers.redraw.swap(false, Ordering::AcqRel) {
            self.inbox.frame_ready.set(true);
        }
        if !loaded.document.has_pending_critical_resources()
            && self.inbox.loaded.get() != Some(providers.generation)
        {
            self.inbox.loaded.set(Some(providers.generation));
            self.inbox.frame_ready.set(true);
        }
    }

    /// Answers the loads Blitz asked for: parts of the current document from memory,
    /// everything else with nothing.
    fn answer(&self, providers: &Providers) {
        let requests = std::mem::take(
            &mut *providers
                .requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for (request, handler) in requests {
            self.answer_one(providers.generation, request, handler);
        }
    }

    fn answer_one(&self, generation: u64, request: Request, handler: Box<dyn NetHandler>) {
        let url = request.url;
        let current = self.current.borrow();
        let document = &current.document;
        let simple_get = request.method == Method::GET && matches!(request.body, Body::Empty);
        let located = resource_for(&url)
            .filter(|located| simple_get && located.generation == generation)
            .filter(|located| located.generation == document.generation());
        let Some(located) = located else {
            return refuse(handler, &url);
        };
        let fetch = match located.resource {
            // The page itself is never loaded as a part of itself (an iframe or an
            // image pointing at it).
            Resource::Document => None,
            Resource::Cid(id) => current.parts.inline_part(&id),
            Resource::Remote(n) => document
                .remote_url(n)
                .and_then(|remote| current.parts.remote(remote)),
        };
        drop(current);
        fetch_in_background(handler, url, fetch);
    }

    fn navigate(&self, generation: u64, navigation: &NavigationOptions) {
        // Forms never submit (the sanitizer removes them; this holds without it).
        if navigation.method != Method::GET || !matches!(navigation.document_resource, Body::Empty)
        {
            return;
        }
        match navigation_for(&navigation.url, generation) {
            Navigation::OpenExternally(url) => self.inbox.open.borrow_mut().push(url),
            // The page stays on its document; Blitz scrolls to fragments itself.
            Navigation::Load | Navigation::Deny => {}
        }
    }

    /// The viewport in device pixels.
    pub(crate) fn resize(&self, width: u32, height: u32) {
        let viewport = {
            let mut viewport = self.viewport.borrow_mut();
            viewport.window_size = (width, height);
            viewport.clone()
        };
        self.set_viewport(viewport);
    }

    pub(crate) fn set_scale(&self, scale: f32) {
        let viewport = {
            let mut viewport = self.viewport.borrow_mut();
            viewport.set_hidpi_scale(scale);
            viewport.clone()
        };
        self.set_viewport(viewport);
    }

    fn set_viewport(&self, viewport: Viewport) {
        if let Some(loaded) = self.loaded.borrow_mut().as_mut() {
            loaded.document.set_viewport(viewport);
        }
        self.inbox.frame_ready.set(true);
    }

    /// Lays out and paints the document into an RGBA image of the viewport's size.
    /// `None` until the document can paint (its blocking resources are in).
    pub(crate) fn paint(&self) -> Option<image::RgbaImage> {
        let mut loaded = self.loaded.borrow_mut();
        let loaded = loaded.as_mut()?;
        let document: &mut BaseDocument = &mut loaded.document;
        document.resolve(self.started.elapsed().as_secs_f64());
        if document.has_pending_critical_resources() {
            return None;
        }
        let viewport = self.viewport.borrow().clone();
        let (width, height) = (viewport.window_size.0.max(1), viewport.window_size.1.max(1));
        let mut renderer = self.renderer.borrow_mut();
        let renderer = match renderer.as_mut() {
            Some((renderer, size)) if *size == (width, height) => renderer,
            _ => {
                &mut renderer
                    .insert((VelloCpuImageRenderer::new(width, height), (width, height)))
                    .0
            }
        };
        // The renderer keeps what it was asked to draw until it's reset: without this,
        // every repaint drew the page over the previous ones: anything
        // semi-transparent got darker with each frame.
        renderer.reset();
        let mut pixels = Vec::new();
        renderer.render_to_vec(
            |scene| {
                blitz_paint::paint_scene(
                    scene,
                    document,
                    viewport.scale_f64(),
                    width,
                    height,
                    0,
                    0,
                );
            },
            &mut pixels,
        );
        // The canvas is white, as in a browser, under a page that sets no background
        // of its own: put the (premultiplied) pixels over white.
        for [red, green, blue, alpha] in pixels.as_chunks_mut::<4>().0 {
            let under = 255 - *alpha;
            for channel in [red, green, blue] {
                *channel = channel.saturating_add(under);
            }
            *alpha = 255;
        }
        image::RgbaImage::from_raw(width, height, pixels)
    }

    /// A pointer move to `point` (CSS pixels in the viewport). Drags select text.
    pub(crate) fn pointer_move(&self, point: (f32, f32)) {
        let buttons = self.with_loaded(|loaded| {
            if loaded.pressed.get() {
                MouseEventButtons::Primary
            } else {
                MouseEventButtons::None
            }
        });
        let Some(buttons) = buttons else {
            return;
        };
        let event = self.pointer_event(point, MouseEventButton::Main, buttons);
        self.dispatch(UiEvent::PointerMove(event));
        self.update_status();
    }

    /// A button goes down or up at `point` (CSS pixels in the viewport).
    pub(crate) fn pointer_button(&self, down: bool, button: MouseEventButton, point: (f32, f32)) {
        if button == MouseEventButton::Main {
            self.with_loaded(|loaded| loaded.pressed.set(down));
        }
        let buttons = if down {
            MouseEventButtons::from(button)
        } else {
            MouseEventButtons::None
        };
        let event = self.pointer_event(point, button, buttons);
        self.dispatch(if down {
            UiEvent::PointerDown(event)
        } else {
            UiEvent::PointerUp(event)
        });
        self.update_status();
    }

    /// A wheel scroll by `delta` CSS pixels (positive `y`: towards the top) at `point`.
    pub(crate) fn wheel(&self, delta: (f64, f64), point: (f32, f32)) {
        let coords = self.coords(point);
        self.dispatch(UiEvent::Wheel(BlitzWheelEvent {
            delta: BlitzWheelDelta::Pixels(delta.0, delta.1),
            coords,
            buttons: MouseEventButtons::None,
            mods: Default::default(),
            element: blitz_traits::events::Point::default(),
        }));
        self.update_status();
    }

    /// The selected text, if any.
    pub(crate) fn selected_text(&self) -> Option<String> {
        self.loaded
            .borrow()
            .as_ref()
            .and_then(|loaded| loaded.document.get_selected_text())
            .filter(|text| !text.is_empty())
    }

    fn with_loaded<T>(&self, f: impl FnOnce(&Loaded) -> T) -> Option<T> {
        self.loaded.borrow().as_ref().map(f)
    }

    fn coords(&self, (x, y): (f32, f32)) -> PointerCoords {
        let (scroll_x, scroll_y) = self
            .loaded
            .borrow()
            .as_ref()
            .map(|loaded| {
                let scroll = loaded.document.viewport_scroll();
                (scroll.x as f32, scroll.y as f32)
            })
            .unwrap_or_default();
        PointerCoords {
            page_x: x + scroll_x,
            page_y: y + scroll_y,
            screen_x: x,
            screen_y: y,
            client_x: x,
            client_y: y,
        }
    }

    fn pointer_event(
        &self,
        point: (f32, f32),
        button: MouseEventButton,
        buttons: MouseEventButtons,
    ) -> BlitzPointerEvent {
        BlitzPointerEvent {
            id: BlitzPointerId::Mouse,
            is_primary: true,
            coords: self.coords(point),
            button,
            buttons,
            mods: Default::default(),
            details: PointerDetails::default(),
            element: blitz_traits::events::Point::default(),
            active_pointers: Default::default(),
        }
    }

    fn dispatch(&self, event: UiEvent) {
        {
            let mut loaded = self.loaded.borrow_mut();
            let Some(loaded) = loaded.as_mut() else {
                return;
            };
            // Hit testing needs an up-to-date layout.
            loaded
                .document
                .resolve(self.started.elapsed().as_secs_f64());
            EventDriver::new(&mut loaded.document, NoopEventHandler).handle_ui_event(event);
        }
        self.poll();
    }

    /// Reports the link under the pointer when it changed (the status text of a
    /// browser).
    fn update_status(&self) {
        let loaded = self.loaded.borrow();
        let Some(loaded) = loaded.as_ref() else {
            return;
        };
        let hover = loaded.document.get_hover_node_id();
        if hover == loaded.hover.get() {
            return;
        }
        let before = loaded
            .hover
            .get()
            .and_then(|id| link_at(&loaded.document, id));
        loaded.hover.set(hover);
        let now = hover.and_then(|id| link_at(&loaded.document, id));
        if before != now {
            *self.inbox.status.borrow_mut() = Some(now);
        }
    }
}

/// The target of the link `node` is in, if any.
fn link_at(document: &BaseDocument, node: blitz_dom::NodeId) -> Option<String> {
    let mut next = Some(node);
    while let Some(id) = next {
        let node = document.get_node(id)?;
        if let Some(element) = node.element_data()
            && element.name.local == blitz_dom::local_name!("a")
            && let Some(href) = element.attr(blitz_dom::local_name!("href"))
        {
            return document.url().join(href).ok().map(String::from);
        }
        next = node.parent;
    }
    None
}

/// Gets a part off the main thread (a blob read, or a remote image Maze downloads)
/// and hands it to Blitz, which decodes it on the same thread. Only images are
/// served.
fn fetch_in_background(handler: Box<dyn NetHandler>, url: Url, fetch: Option<Fetch>) {
    let Some(fetch) = fetch else {
        return refuse(handler, &url);
    };
    let spawned = std::thread::Builder::new()
        .name("original-html-part".into())
        .spawn(move || {
            let bytes = match fetch() {
                Some(part) if part.content_type.starts_with("image/") => Bytes::from(part.bytes),
                _ => Bytes::new(),
            };
            handler.bytes(url.to_string(), bytes);
        });
    if let Err(error) = spawned {
        tracing::warn!(?error, "can't start a thread for a message part");
    }
}

#[cfg(test)]
mod tests {
    //! Blitz itself, headless: the hardening must hold in the real engine.

    use std::io::Read as _;
    use std::net::TcpListener;
    use std::time::Duration;

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

    fn spin_until(page: &Page, timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while !done() {
            if start.elapsed() > timeout {
                return false;
            }
            page.poll();
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    #[test]
    fn blitz_renders_the_message_and_nothing_else() {
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

        let started = Instant::now();
        let inbox = Rc::new(Inbox::default());
        let page = Page::new(
            document,
            Rc::new(Parts),
            inbox.clone(),
            no_wake(),
            (400, 300, 1.),
        );
        page.load();

        assert!(
            spin_until(&page, Duration::from_secs(30), || inbox.loaded.get()
                == Some(1)
                && inbox.frame_ready.get()),
            "the message never finished loading"
        );
        let first_frame = started.elapsed();
        // Images decode asynchronously: paint frames until the inline part shows.
        let mut image = None;
        spin_until(&page, Duration::from_secs(10), || {
            if inbox.frame_ready.replace(false) {
                image = page.paint();
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

        // Hovering the link reports its target; a click on it is reported while
        // the page stays.
        let point = (100., 90.);
        page.pointer_move(point);
        assert_eq!(
            inbox.status.borrow_mut().take(),
            Some(Some("https://shop.example/deal".to_owned()))
        );
        assert_eq!(inbox.cursor.get(), Some(CursorIcon::Pointer));
        page.pointer_button(true, MouseEventButton::Main, point);
        page.pointer_button(false, MouseEventButton::Main, point);
        assert!(
            spin_until(&page, Duration::from_secs(10), || !inbox
                .open
                .borrow()
                .is_empty()),
            "the click wasn't reported"
        );
        assert_eq!(
            inbox.open.borrow().as_slice(),
            &[Url::parse("https://shop.example/deal").unwrap()]
        );
        assert_eq!(page.url(), Some(document_url(1)));
        eprintln!("first frame after {first_frame:?}");

        // The next message reuses the page: a new document number, and loads of the
        // old one are refused.
        let next = OriginalDocument::build(
            "<style>body { margin: 0; background: rgb(0, 0, 255); }</style><p>Next</p>",
            &DocumentOptions::default(),
            2,
        );
        let switched = Instant::now();
        page.switch_to(next, Rc::new(Parts));
        inbox.frame_ready.set(false);
        page.load();
        assert!(
            spin_until(&page, Duration::from_secs(30), || inbox.loaded.get()
                == Some(2)
                && inbox.frame_ready.get()),
            "the next message never loaded"
        );
        let image = page.paint().unwrap();
        let switch = switched.elapsed();
        assert_eq!(image.get_pixel(300, 250).0, [0, 0, 255, 255]);
        eprintln!("next message after {switch:?}");

        // Without any CSP at all a script still doesn't run: Blitz has no script
        // engine.
        let script = r#"<style>body { margin: 0; background: rgb(255, 0, 0); }</style>
            <script>document.body.style.background = "rgb(0, 255, 0)";</script>"#;
        let inbox = Rc::new(Inbox::default());
        let page = Page::new(
            OriginalDocument::with_header_only(script, ""),
            Rc::new(Parts),
            inbox.clone(),
            no_wake(),
            (400, 300, 1.),
        );
        page.load();
        assert!(spin_until(&page, Duration::from_secs(30), || inbox
            .loaded
            .get()
            == Some(0)
            && inbox.frame_ready.get()));
        let image = page.paint().unwrap();
        assert_eq!(image.get_pixel(300, 250).0, [255, 0, 0, 255]);
    }

    /// Parts for the fidelity renders: inline parts from the raw message, remote
    /// images as solid placeholders (layout comes from the HTML's sizes).
    struct RenderParts {
        raw: Vec<u8>,
    }

    impl PartSource for RenderParts {
        fn inline_part(&self, content_id: &str) -> Option<Fetch> {
            let part = mazemail_mime::inline_part(&self.raw, content_id);
            let fallback = placeholder([29, 39, 51]);
            Some(Box::new(move || {
                Some(match part {
                    Some(part) => Part {
                        content_type: part.content_type,
                        bytes: part.bytes,
                    },
                    None => Part {
                        content_type: "image/png".into(),
                        bytes: fallback,
                    },
                })
            }))
        }

        fn remote(&self, url: &str) -> Option<Fetch> {
            let shade = (url.len() * 37 % 120) as u8;
            let bytes = placeholder([120 + shade, 160, 200 - shade / 2]);
            Some(Box::new(move || {
                Some(Part {
                    content_type: "image/png".into(),
                    bytes,
                })
            }))
        }
    }

    fn placeholder(rgb: [u8; 3]) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::from_pixel(8, 8, image::Rgba([rgb[0], rgb[1], rgb[2], 255]))
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    /// Renders a raw message through the real pipeline (parse, sanitize, document,
    /// Blitz) at `width`×`height` CSS pixels, with its parts loaded and decoded.
    fn render_eml(raw: Vec<u8>, (width, height): (u32, u32)) -> image::RgbaImage {
        let message = mazemail_mime::parse(&raw).unwrap();
        let prepared = message
            .prepare_html(&mazemail_mime::html::SanitizeOptions::default())
            .unwrap();
        let options = DocumentOptions {
            remote_allowed: true,
            colors: None,
        };
        let document = OriginalDocument::build(&prepared.sanitized.html, &options, 1);
        let inbox = Rc::new(Inbox::default());
        let page = Page::new(
            document,
            Rc::new(RenderParts { raw }),
            inbox.clone(),
            no_wake(),
            (width, height, 1.),
        );
        page.load();
        assert!(spin_until(&page, Duration::from_secs(30), || inbox
            .loaded
            .get()
            == Some(1)));
        // Let the parts arrive and decode.
        let mut image = page.paint().unwrap();
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            page.poll();
            if inbox.frame_ready.replace(false) {
                image = page.paint().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        image
    }

    /// Share of pixels in `image` that aren't (nearly) white.
    fn ink(image: &image::RgbaImage) -> f64 {
        let inked = image
            .pixels()
            .filter(|pixel| pixel.0[..3].iter().any(|&channel| channel < 240))
            .count();
        inked as f64 / f64::from(image.width() * image.height())
    }

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(path).unwrap()
    }

    /// GitHub's Dependabot digest (anonymized, `tests/fixtures`) puts the whole message
    /// in a `<td style="display: block">`, which Blitz dropped with everything in it:
    /// the page stayed white. A browser centers it: the block has `margin: 0 auto` in
    /// an anonymous cell that takes the spare width, next to an empty spacer cell.
    #[test]
    fn github_dependabot_digest_renders_centered() {
        let image = render_eml(fixture("github_dependabot_digest.eml"), (1000, 1200));
        let ink = ink(&image);
        assert!(
            ink > 0.02,
            "the message rendered (almost) blank: {ink:.4} inked"
        );
        let inked_column = |x: u32| {
            (0..image.height()).any(|y| image.get_pixel(x, y).0[..3].iter().any(|&c| c < 240))
        };
        let left = (0..image.width()).find(|&x| inked_column(x)).unwrap();
        let right = image.width() - 1 - (0..image.width()).rfind(|&x| inked_column(x)).unwrap();
        assert!(
            left.abs_diff(right) <= 16,
            "not centered: {left} px free on the left, {right} px on the right"
        );
    }

    /// Google Flights' price bar (anonymized, `tests/fixtures`): green, yellow and red
    /// segments that meet, with the price marker in the green one. Blitz gave columns
    /// no cell starts in a share of the width (`<td colspan=5>` in a table of two
    /// columns) and moved a cell that overlaps a `rowspan` to new columns, so the
    /// segments came out short with white gaps between them.
    #[test]
    fn google_flights_price_bar_is_continuous() {
        const GREEN: [u8; 3] = [0x34, 0xa8, 0x53];
        const YELLOW: [u8; 3] = [0xf9, 0xab, 0x00];
        const RED: [u8; 3] = [0xd9, 0x30, 0x25];
        const MARKER: [u8; 3] = [0x1a, 0x73, 0xe8];
        let near = |pixel: &image::Rgba<u8>, color: [u8; 3]| {
            pixel.0[..3]
                .iter()
                .zip(color)
                .all(|(&channel, wanted)| channel.abs_diff(wanted) <= 16)
        };
        let image = render_eml(fixture("google_flights_price_tracking.eml"), (700, 1200));
        // The bar's row: the one with the most yellow.
        let y = (0..image.height())
            .max_by_key(|&y| {
                (0..image.width())
                    .filter(|&x| near(image.get_pixel(x, y), YELLOW))
                    .count()
            })
            .unwrap();
        let bar: Vec<u32> = (0..image.width())
            .filter(|&x| {
                let pixel = image.get_pixel(x, y);
                [GREEN, YELLOW, RED, MARKER]
                    .into_iter()
                    .any(|color| near(pixel, color))
            })
            .collect();
        // The segments sit 2 px apart; the marker's white center is 8 px wide.
        let widest_gap = bar.windows(2).map(|pair| pair[1] - pair[0]).max().unwrap();
        assert!(
            widest_gap <= 12,
            "a {widest_gap} px gap in the price bar (row {y})"
        );
        let width = bar.last().unwrap() - bar.first().unwrap();
        assert!(width > 350, "the price bar is {width} px wide");
    }

    /// Renders mail through the real pipeline (parse, sanitize, document) to PNGs in
    /// `$MAZEMAIL_RENDER_OUT`, to compare Blitz's layout of mail by eye.
    #[test]
    #[ignore = "writes PNGs; run with MAZEMAIL_RENDER_OUT=<dir>"]
    fn render_mail_to_png() {
        let out = std::path::PathBuf::from(std::env::var("MAZEMAIL_RENDER_OUT").unwrap());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut mails: Vec<(String, Vec<u8>)> = [
            "newsletter.eml",
            "outlook_reply.eml",
            "attachment_inline_cid.eml",
            "multipart_alternative.eml",
        ]
        .iter()
        .map(|name| {
            let path = root.join("mazemail_mime/tests/fixtures").join(name);
            (
                name.trim_end_matches(".eml").to_owned(),
                std::fs::read(path).unwrap(),
            )
        })
        .collect();
        for name in ["table_newsletter", "css_equivalents", "legacy_attributes"] {
            let html = fixture(&format!("{name}.html"));
            let mut eml = b"From: Acme <news@acme.example.com>\r\nTo: anna@example.com\r\nSubject: Spring\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n".to_vec();
            eml.extend_from_slice(&html);
            mails.push((name.to_owned(), eml));
        }
        for name in ["github_dependabot_digest", "google_flights_price_tracking"] {
            mails.push((name.to_owned(), fixture(&format!("{name}.eml"))));
        }
        // Mail from outside the repo (one that renders wrongly), never committed:
        // `MAZEMAIL_RENDER_EML=<file.eml>[:<file.eml>…]`.
        if let Some(paths) = std::env::var_os("MAZEMAIL_RENDER_EML") {
            for path in std::env::split_paths(&paths) {
                let name = path.file_stem().unwrap().to_string_lossy().into_owned();
                mails.push((name, std::fs::read(&path).unwrap()));
            }
        }

        for (name, raw) in mails {
            let started = Instant::now();
            let image = render_eml(raw, (700, 1700));
            eprintln!("{name}: first paint path {:?}", started.elapsed());
            image.save(out.join(format!("{name}.png"))).unwrap();
        }
    }

    /// A semi-transparent image (the fixture's inline part is blue at 50% alpha) stays
    /// the same colour however often the page repaints: the renderer drew every frame
    /// over the previous ones, so it came out (64, 64, 255) instead of (128, 128, 255).
    #[test]
    fn repaints_dont_stack() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let raw =
            std::fs::read(root.join("mazemail_mime/tests/fixtures/attachment_inline_cid.eml"))
                .unwrap();
        let message = mazemail_mime::parse(&raw).unwrap();
        let prepared = message
            .prepare_html(&mazemail_mime::html::SanitizeOptions::default())
            .unwrap();
        let options = DocumentOptions {
            remote_allowed: true,
            colors: None,
        };
        let document = OriginalDocument::build(&prepared.sanitized.html, &options, 1);
        let inbox = Rc::new(Inbox::default());
        let page = Page::new(
            document,
            Rc::new(RenderParts { raw }),
            inbox.clone(),
            no_wake(),
            (400, 200, 1.),
        );
        page.load();
        assert!(spin_until(&page, Duration::from_secs(30), || inbox
            .loaded
            .get()
            == Some(1)));
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            page.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..5 {
            let image = page.paint().unwrap();
            // The 120×40 image sits at the top left, below the paragraph margin.
            let pixel = image.get_pixel(60, 35).0;
            assert!(
                pixel[0].abs_diff(128) <= 2 && pixel[1].abs_diff(128) <= 2 && pixel[2] >= 253,
                "50% blue over white should be about (128, 128, 255), not {pixel:?}"
            );
        }
    }

    /// Paint cost of a table newsletter: a full paint, a hover move that crosses
    /// elements and a wheel scroll, each followed by the repaint the view does. Medians
    /// of 20 at 1× (700×900) and 2× (1400×1800, a Retina display). Compare the dev
    /// profile with `--profile release-fast`.
    #[test]
    #[ignore = "timing; run with --nocapture"]
    fn paint_time() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let html =
            std::fs::read(root.join("html_view/tests/fixtures/table_newsletter.html")).unwrap();
        let mut raw = b"From: Acme <news@acme.example.com>\r\nTo: anna@example.com\r\nSubject: Spring\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n".to_vec();
        raw.extend_from_slice(&html);
        let message = mazemail_mime::parse(&raw).unwrap();
        let prepared = message
            .prepare_html(&mazemail_mime::html::SanitizeOptions::default())
            .unwrap();
        let options = DocumentOptions {
            remote_allowed: true,
            colors: None,
        };

        fn median(mut samples: Vec<Duration>) -> Duration {
            samples.sort();
            samples[samples.len() / 2]
        }

        for (width, height, scale) in [(700, 900, 1.), (1400, 1800, 2.)] {
            let document = OriginalDocument::build(&prepared.sanitized.html, &options, 1);
            let inbox = Rc::new(Inbox::default());
            let page = Page::new(
                document,
                Rc::new(RenderParts { raw: raw.clone() }),
                inbox.clone(),
                no_wake(),
                (width, height, scale),
            );
            page.load();
            assert!(spin_until(&page, Duration::from_secs(30), || inbox
                .loaded
                .get()
                == Some(1)));
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                page.poll();
                std::thread::sleep(Duration::from_millis(5));
            }
            page.paint().unwrap();

            let full = median(
                (0..20)
                    .map(|_| {
                        let start = Instant::now();
                        page.paint().unwrap();
                        start.elapsed()
                    })
                    .collect(),
            );
            // Down the page in CSS pixels: every step lands on another element.
            let hover = median(
                (0..20)
                    .map(|i| {
                        let start = Instant::now();
                        page.pointer_move((120. + i as f32 * 7., 200. + i as f32 * 31.));
                        page.paint().unwrap();
                        start.elapsed()
                    })
                    .collect(),
            );
            let scroll = median(
                (0..20)
                    .map(|i| {
                        let start = Instant::now();
                        let delta = if i % 2 == 0 { -40. } else { 40. };
                        page.wheel((0., delta), (300., 300.));
                        page.paint().unwrap();
                        start.elapsed()
                    })
                    .collect(),
            );
            eprintln!(
                "{width}x{height} @{scale}x: full paint {full:?}, hover move + paint {hover:?}, scroll + paint {scroll:?}"
            );
        }
    }

    #[test]
    fn selected_text_can_be_copied() {
        let document = OriginalDocument::build(
            "<style>body { margin: 0; font-size: 20px; }</style><p>Copy me please</p>",
            &DocumentOptions::default(),
            1,
        );
        let inbox = Rc::new(Inbox::default());
        let page = Page::new(
            document,
            Rc::new(Parts),
            inbox.clone(),
            no_wake(),
            (400, 300, 1.),
        );
        page.load();
        assert!(spin_until(&page, Duration::from_secs(30), || inbox
            .loaded
            .get()
            == Some(1)));
        assert_eq!(page.selected_text(), None);
        // Drag across the paragraph's first line.
        page.pointer_button(true, MouseEventButton::Main, (1., 30.));
        page.pointer_move((390., 30.));
        page.pointer_button(false, MouseEventButton::Main, (390., 30.));
        assert_eq!(page.selected_text().as_deref(), Some("Copy me please"));
    }
}

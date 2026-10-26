//! [`OriginalView`]: a message's original HTML as a normal GPUI element.
//!
//! Servo renders into an offscreen software context; each new frame is copied into a
//! GPUI image and painted in the view's bounds. Mouse and wheel events on the view go
//! to Servo. Because it is an ordinary element (no native view), it clips, layers
//! under overlays, hides with its tab and leaves keyboard focus in GPUI.
//!
//! One view shows one message at a time and keeps its Servo page for the next one
//! ([`OriginalView::show`]): each document gets a new number, the page loads it after a
//! short pause (holding `f` through the list doesn't queue loads), and loads of a
//! document the view has moved on from are refused. Until the current document has
//! painted, [`OriginalView::is_ready`] is false and the caller shows the reader.
//!
//! Without the `servo` feature the view never gets ready; check
//! [`OriginalView::AVAILABLE`].

use std::rc::Rc;
use std::time::Instant;

use gpui::{Context, EventEmitter, IntoElement, Render, Window};
use url::Url;

use crate::document::{DocumentOptions, OriginalDocument};

/// A message part served to the engine from memory.
#[derive(Clone, PartialEq, Eq)]
pub struct Part {
    /// MIME type, e.g. `image/png`.
    pub content_type: String,
    /// The decoded bytes.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Part {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Part")
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// Gets a part; runs on a background thread, so the UI never waits for a blob read or
/// a download.
pub type Fetch = Box<dyn FnOnce() -> Option<Part> + Send>;

/// Where the parts of a message come from.
pub trait PartSource: 'static {
    /// The inline part with this Content-ID (without angle brackets). `None`: refuse
    /// at once.
    fn inline_part(&self, content_id: &str) -> Option<Fetch>;

    /// A remote resource of the message, fetched by Maze. Only called when the
    /// document allows remote resources ([`DocumentOptions::remote_allowed`]).
    fn remote(&self, url: &str) -> Option<Fetch>;
}

/// What the view reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginalViewEvent {
    /// A link was clicked (`http`, `https` or `mailto`; the navigation policy denied
    /// everything else). Open it outside Maze after the link checks.
    OpenLink(Url),
    /// The pointer is over a link (its target), or left it.
    Hover(Option<String>),
    /// The current document has painted: stop showing the fallback.
    Ready,
}

/// The document to show and where its parts come from.
struct Shown {
    document: OriginalDocument,
    #[cfg_attr(not(feature = "servo"), allow(dead_code))]
    parts: Rc<dyn PartSource>,
    #[cfg_attr(not(feature = "servo"), allow(dead_code))]
    since: Instant,
}

/// A message's original HTML.
pub struct OriginalView {
    generation: u64,
    shown: Option<Shown>,
    #[cfg(feature = "servo")]
    servo: Option<servo_view::State>,
    #[cfg(feature = "servo")]
    pending_load: Option<gpui::Task<()>>,
    failed: bool,
}

impl std::fmt::Debug for OriginalView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalView")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<OriginalViewEvent> for OriginalView {}

impl OriginalView {
    /// Whether this build includes the engine (feature `servo`).
    pub const AVAILABLE: bool = cfg!(feature = "servo");

    /// How long a new document waits before it loads, so quickly passing messages
    /// don't each start a load.
    pub const LOAD_DELAY: std::time::Duration = std::time::Duration::from_millis(40);

    /// An empty view. The engine starts when the view is first painted with a
    /// document.
    pub fn new(_cx: &mut Context<'_, Self>) -> Self {
        Self {
            generation: 0,
            shown: None,
            #[cfg(feature = "servo")]
            servo: None,
            #[cfg(feature = "servo")]
            pending_load: None,
            failed: false,
        }
    }

    /// Shows a message: builds its document from the sanitized HTML and loads it
    /// (after [`Self::LOAD_DELAY`] when the page shows another message already).
    pub fn show(
        &mut self,
        sanitized_html: &str,
        options: &DocumentOptions,
        parts: Rc<dyn PartSource>,
        cx: &mut Context<'_, Self>,
    ) {
        self.generation += 1;
        let document = OriginalDocument::build(sanitized_html, options, self.generation);
        #[cfg(feature = "servo")]
        if let Some(state) = &self.servo {
            state.switch_to(document.clone(), parts.clone());
            let generation = self.generation;
            self.pending_load = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(Self::LOAD_DELAY).await;
                let _ = this.update(cx, |view, _| {
                    if view.generation == generation
                        && let (Some(state), Some(shown)) = (&view.servo, &view.shown)
                    {
                        state.load(shown.document.url());
                    }
                });
            }));
        }
        self.shown = Some(Shown {
            document,
            parts,
            since: Instant::now(),
        });
        cx.notify();
    }

    /// The document shown (or loading).
    pub fn document(&self) -> Option<&OriginalDocument> {
        self.shown.as_ref().map(|shown| &shown.document)
    }

    /// Whether the current document has painted. Until then, show the reader.
    pub fn is_ready(&self) -> bool {
        #[cfg(feature = "servo")]
        return self
            .servo
            .as_ref()
            .is_some_and(|state| state.frame_generation() == Some(self.generation));
        #[cfg(not(feature = "servo"))]
        false
    }

    /// Whether the engine is missing or couldn't start (the caller stays in reader
    /// mode).
    pub fn failed(&self) -> bool {
        self.failed || !Self::AVAILABLE
    }

    /// Reports a click on `url` (already allowed by the navigation policy).
    pub fn open_link(&mut self, url: Url, cx: &mut Context<'_, Self>) {
        cx.emit(OriginalViewEvent::OpenLink(url));
    }

    /// Copies the selected text to the clipboard.
    pub fn copy(&mut self, cx: &mut Context<'_, Self>) {
        #[cfg(feature = "servo")]
        if let Some(state) = &self.servo {
            state.copy();
            cx.notify();
        }
        #[cfg(not(feature = "servo"))]
        let _ = cx;
    }

    #[cfg(feature = "servo")]
    pub(crate) fn after_spin(&mut self, cx: &mut Context<'_, Self>) {
        let Some(state) = &self.servo else {
            return;
        };
        let (opens, clipboard, redraw) = state.take_inbox();
        if let Some(hover) = state.take_hover() {
            cx.emit(OriginalViewEvent::Hover(hover));
        }
        if let Some(text) = clipboard {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
        let waiting = state.loaded_but_not_painted();
        for url in opens {
            self.open_link(url, cx);
        }
        if redraw || waiting {
            cx.notify();
        }
    }

    /// Called once a frame of the current document is on screen.
    #[cfg(feature = "servo")]
    fn became_ready(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(shown) = &self.shown {
            tracing::debug!(
                elapsed_ms = shown.since.elapsed().as_secs_f64() * 1e3,
                generation = self.generation,
                "original HTML: first frame of the message"
            );
        }
        cx.emit(OriginalViewEvent::Ready);
    }
}

impl Render for OriginalView {
    #[cfg(feature = "servo")]
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        servo_view::render(self, window, cx)
    }

    #[cfg(not(feature = "servo"))]
    fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
        gpui::Empty
    }
}

#[cfg(feature = "servo")]
mod servo_view {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;

    use gpui::{
        Bounds, Context, CursorStyle, InteractiveElement as _, IntoElement, MouseButton,
        ParentElement as _, Pixels, RenderImage, ScrollDelta, Styled as _, Window, canvas, div, px,
    };
    use image::Frame;
    use servo::{
        Cursor, DeviceIntRect, DevicePoint, InputEvent, MouseButtonAction, MouseButtonEvent,
        MouseMoveEvent, RenderingContext as _, SoftwareRenderingContext, WebView, WebViewBuilder,
        WheelDelta, WheelEvent, WheelMode,
    };

    use super::{OriginalView, PartSource};
    use crate::document::OriginalDocument;
    use crate::engine::{Clipboard, Current, Delegate, Engine, Inbox};

    /// A wheel "line" in pixels, as Servo's own shell uses.
    const LINE_HEIGHT: f32 = 38.;

    pub(super) struct State {
        webview: WebView,
        context: Rc<SoftwareRenderingContext>,
        delegate: Rc<Delegate>,
        inbox: Rc<Inbox>,
        /// Device size the page was laid out for.
        size: Cell<(u32, u32)>,
        scale: Cell<f32>,
        /// Window-relative bounds of the view, for event coordinates.
        bounds: Cell<Bounds<Pixels>>,
        /// The last frame and the document it shows.
        frame: RefCell<Option<(u64, Arc<RenderImage>)>>,
        /// Frames waiting to be read out of the rendering context.
        dirty: Cell<bool>,
    }

    impl State {
        fn new(
            document: OriginalDocument,
            parts: Rc<dyn PartSource>,
            (width, height, scale): (u32, u32, f32),
            cx: &mut gpui::App,
        ) -> Option<Self> {
            let context = match SoftwareRenderingContext::new(dpi::PhysicalSize::new(width, height))
            {
                Ok(context) => Rc::new(context),
                Err(error) => {
                    tracing::warn!(?error, "can't create Servo's software rendering context");
                    return None;
                }
            };
            let inbox = Rc::new(Inbox::default());
            let url = document.url();
            let engine = Engine::get(cx);
            let delegate = Rc::new(Delegate::new(document, parts, inbox.clone(), engine.wake()));
            let webview = WebViewBuilder::new(engine.servo(), context.clone())
                .delegate(delegate.clone())
                .clipboard_delegate(Rc::new(Clipboard {
                    inbox: inbox.clone(),
                }))
                .hidpi_scale_factor(euclid::Scale::new(scale))
                .url(url)
                .build();
            Some(Self {
                webview,
                context,
                delegate,
                inbox,
                size: Cell::new((width, height)),
                scale: Cell::new(scale),
                bounds: Cell::new(Bounds::default()),
                frame: RefCell::new(None),
                dirty: Cell::new(false),
            })
        }

        /// Makes `document` the current one at once (so loads of the previous one are
        /// refused from now on); the page loads it with [`State::load`].
        pub(super) fn switch_to(&self, document: OriginalDocument, parts: Rc<dyn PartSource>) {
            *self.delegate.current.borrow_mut() = Current { document, parts };
        }

        pub(super) fn load(&self, url: url::Url) {
            self.webview.load(url);
        }

        /// The link under the pointer, if it changed since the last call.
        pub(super) fn take_hover(&self) -> Option<Option<String>> {
            self.inbox.status.borrow_mut().take()
        }

        /// The current document finished loading but hasn't been painted yet.
        pub(super) fn loaded_but_not_painted(&self) -> bool {
            let current = self.delegate.current.borrow().document.generation();
            if self.inbox.loaded.get() == Some(current) && self.frame_generation() != Some(current)
            {
                self.dirty.set(true);
                true
            } else {
                false
            }
        }

        pub(super) fn frame_generation(&self) -> Option<u64> {
            self.frame
                .borrow()
                .as_ref()
                .map(|(generation, _)| *generation)
        }

        pub(super) fn copy(&self) {
            self.send(InputEvent::EditingAction(servo::ClipboardAction::Copy));
        }

        /// Links to open, clipboard text, and whether to redraw.
        pub(super) fn take_inbox(&self) -> (Vec<url::Url>, Option<String>, bool) {
            self.delegate.poll_pending();
            let opens = std::mem::take(&mut *self.inbox.open.borrow_mut());
            let clipboard = self.inbox.clipboard.borrow_mut().take();
            let frame = self.inbox.frame_ready.replace(false);
            if frame {
                self.dirty.set(true);
            }
            let cursor_changed = self.inbox.cursor.get().is_some();
            (opens, clipboard, frame || cursor_changed)
        }

        fn layout(&self, bounds: Bounds<Pixels>, scale: f32) {
            self.bounds.set(bounds);
            let (width, height) = device_size(bounds, scale);
            if (width, height) != self.size.get() {
                self.size.set((width, height));
                self.webview.resize(dpi::PhysicalSize::new(width, height));
            }
            if scale != self.scale.get() {
                self.scale.set(scale);
                self.webview
                    .set_hidpi_scale_factor(euclid::Scale::new(scale));
            }
        }

        /// Paints the pending frame and copies it out, if it shows the current
        /// document (frames of the previous message are never shown). Returns whether
        /// this was the current document's first frame.
        fn read_frame(&self, window: &mut Window, cx: &mut gpui::App) -> bool {
            let current = self.delegate.current.borrow().document.generation();
            if self.inbox.loaded.get() != Some(current) || !self.dirty.replace(false) {
                return false;
            }
            self.webview.paint();
            let (width, height) = self.size.get();
            let rect =
                DeviceIntRect::from_size(servo::DeviceIntSize::new(width as i32, height as i32));
            let Some(mut rgba) = self.context.read_to_image(rect) else {
                return false;
            };
            // GPUI images are BGRA.
            for pixel in rgba.pixels_mut() {
                pixel.0.swap(0, 2);
            }
            let image = Arc::new(RenderImage::new(vec![Frame::new(rgba)]));
            let first = self.frame_generation() != Some(current);
            if let Some((_, old)) = self.frame.borrow_mut().replace((current, image)) {
                cx.drop_image(old, Some(window));
            }
            first
        }

        fn point(&self, position: gpui::Point<Pixels>) -> servo::WebViewPoint {
            let origin = self.bounds.get().origin;
            let scale = self.scale.get();
            DevicePoint::new(
                f32::from(position.x - origin.x) * scale,
                f32::from(position.y - origin.y) * scale,
            )
            .into()
        }

        fn cursor_style(&self) -> CursorStyle {
            match self.inbox.cursor.get() {
                Some(Cursor::Pointer) => CursorStyle::PointingHand,
                Some(Cursor::Text | Cursor::VerticalText) => CursorStyle::IBeam,
                Some(Cursor::Crosshair) => CursorStyle::Crosshair,
                Some(Cursor::NotAllowed | Cursor::NoDrop) => CursorStyle::OperationNotAllowed,
                _ => CursorStyle::Arrow,
            }
        }

        fn send(&self, event: InputEvent) {
            self.webview.notify_input_event(event);
        }
    }

    impl Drop for State {
        fn drop(&mut self) {
            // The page goes when its last handle does; stop it painting meanwhile.
            self.webview.hide();
        }
    }

    fn device_size(bounds: Bounds<Pixels>, scale: f32) -> (u32, u32) {
        let side = |logical: Pixels| (f32::from(logical) * scale).round().max(1.) as u32;
        (side(bounds.size.width), side(bounds.size.height))
    }

    fn button(button: MouseButton) -> Option<servo::MouseButton> {
        match button {
            MouseButton::Left => Some(servo::MouseButton::Primary),
            MouseButton::Middle => Some(servo::MouseButton::Auxiliary),
            _ => None,
        }
    }

    fn mouse_button(
        view: &OriginalView,
        action: MouseButtonAction,
        pressed: MouseButton,
        position: gpui::Point<Pixels>,
    ) {
        if let (Some(state), Some(button)) = (&view.servo, button(pressed)) {
            state.send(InputEvent::MouseButton(MouseButtonEvent::new(
                action,
                button,
                state.point(position),
            )));
        }
    }

    pub(super) fn render(
        view: &mut OriginalView,
        _window: &mut Window,
        cx: &mut Context<'_, OriginalView>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let cursor = view
            .servo
            .as_ref()
            .map_or(CursorStyle::Arrow, State::cursor_style);
        div()
            .id("original-html")
            .size_full()
            .overflow_hidden()
            .cursor(cursor)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, _| {
                    mouse_button(this, MouseButtonAction::Down, event.button, event.position);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseUpEvent, _, _| {
                    mouse_button(this, MouseButtonAction::Up, event.button, event.position);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, _| {
                if let Some(state) = &this.servo {
                    state.send(InputEvent::MouseMove(MouseMoveEvent::new(
                        state.point(event.position),
                    )));
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, _| {
                if let Some(state) = &this.servo {
                    let delta = match event.delta {
                        ScrollDelta::Pixels(delta) => delta,
                        ScrollDelta::Lines(lines) => {
                            gpui::point(px(lines.x * LINE_HEIGHT), px(lines.y * LINE_HEIGHT))
                        }
                    };
                    let scale = f64::from(state.scale.get());
                    state.send(InputEvent::Wheel(WheelEvent::new(
                        WheelDelta {
                            x: f64::from(f32::from(delta.x)) * scale,
                            y: f64::from(f32::from(delta.y)) * scale,
                            z: 0.,
                            mode: WheelMode::DeltaPixel,
                        },
                        state.point(event.position),
                    )));
                }
            }))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let scale = window.scale_factor();
                        entity.update(cx, |view, cx| {
                            if view.servo.is_none() && !view.failed {
                                let shown = view.shown.as_ref()?;
                                let size = device_size(bounds, scale);
                                view.servo = State::new(
                                    shown.document.clone(),
                                    shown.parts.clone(),
                                    (size.0, size.1, scale),
                                    cx,
                                );
                                if view.servo.is_some() {
                                    let weak = cx.entity().downgrade();
                                    Engine::get(cx).register(weak);
                                } else {
                                    view.failed = true;
                                    cx.notify();
                                }
                            }
                            let state = view.servo.as_ref()?;
                            state.layout(bounds, scale);
                            let first = state.read_frame(window, cx);
                            let frame = state.frame.borrow().clone();
                            if first {
                                view.became_ready(cx);
                            }
                            frame
                                .filter(|(generation, _)| *generation == view.generation)
                                .map(|(_, image)| image)
                        })
                    },
                    move |bounds, frame, window, _| {
                        if let Some(frame) = frame {
                            let _ = window.paint_image(
                                bounds,
                                bounds,
                                gpui::Corners::default(),
                                frame,
                                0,
                                false,
                            );
                        }
                    },
                )
                .size_full(),
            )
    }
}

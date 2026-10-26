//! The message view: header with sender (and their mail client), recipients, date and
//! actions, the verification row, attachments, and the body (CONCEPT §8.4).
//!
//! A right-click opens the message list's context menu for the shown message (the
//! window sets it, [`MessageView::set_context_menu`]), plus Copy Sender Address.
//!
//! Hovering a link shows its target at the bottom left; a click opens it after the
//! checks of [`crate::links`] (normalized, web and `mailto:` only, deceptive links ask).

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, ClipboardItem, Context, FocusHandle, Focusable, HighlightStyle, InteractiveElement,
    InteractiveText, IntoElement, MouseButton, MouseMoveEvent, ParentElement, Pixels, Point,
    Render, SharedString, StatefulInteractiveElement as _, Styled, StyledText, UnderlineStyle,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _,
    button::Button,
    h_flex,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    text::TextView,
    v_flex,
};
use mazemail_types::Address;
use time::UtcOffset;
use ui::assets::IconName;

use crate::actions::{Archive, Forward, Reply, ReplyAll};
use crate::format::{DateFormatter, format_size, format_thousands, initials};
use crate::links::{HoveredLink, LinkPreview, TextViewLinkProbe, detect_links, open_link};
use crate::source::{BodyLink, MessageBody, MessageDocument, SenderClient, Verdict, Verification};

/// Builds a context menu (see [`MessageView::set_context_menu`]).
pub type MenuBuilder = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<'_, PopupMenu>) -> PopupMenu>;

/// The message view.
pub struct MessageView {
    focus: FocusHandle,
    offset: UtcOffset,
    document: Option<MessageDocument>,
    error: Option<SharedString>,
    /// Rows selected in the list, when there are several (no message is shown then).
    selection: Option<usize>,
    context_menu: Option<MenuBuilder>,
    /// The link under the pointer, shown at the bottom left.
    hovered: Option<HoveredLink>,
    /// Finds the hovered link in the HTML and Markdown body.
    probe: TextViewLinkProbe,
    /// Where the pointer was when the body was last probed for a link.
    probed_at: Option<Point<Pixels>>,
}

impl std::fmt::Debug for MessageView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageView").finish_non_exhaustive()
    }
}

impl MessageView {
    /// An empty message view.
    pub fn new(offset: UtcOffset, cx: &mut Context<'_, Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            offset,
            document: None,
            error: None,
            selection: None,
            context_menu: None,
            hovered: None,
            probe: TextViewLinkProbe::default(),
            probed_at: None,
        }
    }

    /// The menu a right-click on a shown message opens: the message list's, for its
    /// selection (which is the shown message). Copy Sender Address is added to it.
    pub fn set_context_menu(&mut self, builder: MenuBuilder) {
        self.context_menu = Some(builder);
    }

    /// Shows a message, or the empty state.
    pub fn show(&mut self, document: Option<MessageDocument>, cx: &mut Context<'_, Self>) {
        self.document = document;
        self.error = None;
        self.selection = None;
        self.hovered = None;
        self.probed_at = None;
        cx.notify();
    }

    /// Shows an error instead of a message.
    pub fn show_error(&mut self, error: impl Into<SharedString>, cx: &mut Context<'_, Self>) {
        self.document = None;
        self.error = Some(error.into());
        self.selection = None;
        self.hovered = None;
        self.probed_at = None;
        cx.notify();
    }

    /// Shows "N selected" instead of a message: several rows are selected in the list.
    pub fn show_selection(&mut self, count: usize, cx: &mut Context<'_, Self>) {
        self.document = None;
        self.error = None;
        self.selection = Some(count);
        self.hovered = None;
        self.probed_at = None;
        cx.notify();
    }

    /// The number of selected rows shown instead of a message, if any.
    pub fn selection(&self) -> Option<usize> {
        self.selection
    }

    /// The UTC offset dates are shown in.
    pub fn offset(&self) -> UtcOffset {
        self.offset
    }

    /// The displayed message.
    pub fn document(&self) -> Option<&MessageDocument> {
        self.document.as_ref()
    }

    /// The link under the pointer, as the preview at the bottom left shows it.
    pub fn hovered_link(&self) -> Option<&HoveredLink> {
        self.hovered.as_ref()
    }

    /// Shows where the hovered link goes, or hides the preview (`None`). Body renderers
    /// call this when the pointer enters or leaves a link; the shown message's links
    /// tell whether it is deceptive.
    pub fn hover_link(&mut self, href: Option<&str>, cx: &mut Context<'_, Self>) {
        let links = self
            .document
            .as_ref()
            .map(|doc| doc.links.expose().as_slice())
            .unwrap_or_default();
        let link = href
            .filter(|href| !href.trim().is_empty())
            .map(|href| HoveredLink::resolve(href, links));
        if self.hovered != link {
            self.hovered = link;
            cx.notify();
        }
    }

    /// The pointer moved over the body: look for a link under it in the HTML or
    /// Markdown body (the plain-text body reports its links itself).
    fn on_body_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let rich = matches!(
            self.document.as_ref().map(|doc| &doc.body),
            Some(MessageBody::Html(_) | MessageBody::Markdown(_))
        );
        if !rich || event.pressed_button.is_some() || self.probed_at == Some(event.position) {
            return;
        }
        self.probed_at = Some(event.position);
        let (probe, position, view) = (self.probe.clone(), event.position, cx.weak_entity());
        // The probe dispatches an event itself, which it can't do from inside this one.
        window.defer(cx, move |window, cx| {
            if window.mouse_position() != position {
                // The pointer moved on; that move probes again.
                return;
            }
            let href = probe.link_at(position, window, cx);
            view.update(cx, |this, cx| this.hover_link(href.as_deref(), cx))
                .ok();
        });
    }

    /// Plain text with its links (web and `mailto:`) underlined, clickable and
    /// previewed on hover.
    fn render_plain_text(
        text: SharedString,
        links: Arc<[BodyLink]>,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let link_color = cx.theme().link;
        let detected = detect_links(&text);
        let style = HighlightStyle {
            color: Some(link_color),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(link_color),
                wavy: false,
            }),
            ..HighlightStyle::default()
        };
        let ranges: Rc<[std::ops::Range<usize>]> =
            detected.iter().map(|(range, _)| range.clone()).collect();
        let hrefs: Rc<[SharedString]> = detected.iter().map(|(_, href)| href.clone()).collect();
        let text = StyledText::new(text).with_highlights(ranges.iter().map(|r| (r.clone(), style)));
        let view = cx.weak_entity();
        let click_hrefs = hrefs.clone();
        let hover_ranges = ranges.clone();
        InteractiveText::new("message-plain-text", text)
            .on_click(ranges.to_vec(), move |ix, window, cx| {
                if let Some(href) = click_hrefs.get(ix) {
                    open_link(href, &links, window, cx);
                }
            })
            .on_hover(move |ix, _, _, cx| {
                let href = ix
                    .and_then(|ix| hover_ranges.iter().position(|range| range.contains(&ix)))
                    .and_then(|link| hrefs.get(link).cloned());
                view.update(cx, |this, cx| this.hover_link(href.as_deref(), cx))
                    .ok();
            })
    }

    fn address_line(label: &'static str, addresses: &[Address], cx: &App) -> impl IntoElement {
        let text = addresses
            .iter()
            .map(|a| a.name.clone().unwrap_or_else(|| a.email.clone()))
            .collect::<Vec<_>>()
            .join(", ");
        h_flex()
            .gap_2()
            .text_xs()
            .child(
                div()
                    .w(px(36.))
                    .flex_none()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(div().truncate().child(text))
    }

    fn render_header(&self, doc: &MessageDocument, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let dates = DateFormatter::new(self.offset);
        let from = doc.from.expose();
        let name: SharedString = from
            .name
            .clone()
            .unwrap_or_else(|| from.email.clone())
            .into();
        let action = |id: &'static str, label: &'static str, icon: IconName| {
            Button::new(id).outline().xsmall().icon(icon).label(label)
        };
        v_flex()
            .flex_none()
            .gap_2()
            .px_4()
            .pt_3()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_lg()
                    .font_semibold()
                    .line_clamp(2)
                    .child(doc.subject.expose().clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .size(px(32.))
                            .flex_none()
                            .rounded_full()
                            .bg(theme.primary.opacity(0.2))
                            .text_color(theme.primary)
                            .text_xs()
                            .font_semibold()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(initials(&name)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_semibold().truncate().child(name))
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .gap_1()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(div().min_w_0().truncate().child(from.email.clone()))
                                    .when_some(doc.client.expose().clone(), |this, client| {
                                        this.child(Self::render_client(client))
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(dates.long(doc.date)),
                    ),
            )
            .child(Self::address_line("To", doc.to.expose(), cx))
            .when(!doc.cc.expose().is_empty(), |this| {
                this.child(Self::address_line("Cc", doc.cc.expose(), cx))
            })
            .child(Self::render_verification(&doc.verification, cx))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        action("reply", "Reply", IconName::Reply)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Reply), cx)),
                    )
                    .child(
                        action("reply-all", "All", IconName::ReplyAll).on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(ReplyAll), cx)
                        }),
                    )
                    .child(
                        action("forward", "Fwd", IconName::Forward).on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(Forward), cx)
                        }),
                    )
                    .child(
                        action("archive", "Archive", IconName::Archive).on_click(
                            |_, window, cx| window.dispatch_action(Box::new(Archive), cx),
                        ),
                    ),
            )
    }

    /// "· via Thunderbird 128.3" after the sender's address, with the header it was
    /// recognized by as a tooltip.
    fn render_client(client: SenderClient) -> impl IntoElement {
        div()
            .id("sender-client")
            .flex_shrink_0()
            .truncate()
            .child(SharedString::from(format!("· via {}", client.name)))
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(client.header.clone()).build(window, cx)
            })
    }

    /// The verification row: DKIM, SPF and DMARC results and the verdict, with the
    /// explanation as a tooltip.
    fn render_verification(verification: &Verification, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        let chip = |label: &'static str, result: SharedString| {
            let color = match result.as_ref() {
                "pass" => theme.success,
                "fail" => theme.danger,
                _ => theme.muted_foreground,
            };
            h_flex()
                .gap_0p5()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(theme.border)
                .child(div().text_color(theme.muted_foreground).child(label))
                .child(div().text_color(color).child(result))
        };
        let row = h_flex().id("verification").gap_1p5().text_xs();
        match verification {
            Verification::Pending | Verification::Unavailable => {
                let text = if *verification == Verification::Pending {
                    "Checking the sender…"
                } else {
                    "Sender can't be verified (no DNS)"
                };
                row.child(
                    Icon::new(IconName::ShieldCheck)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(chip("DKIM", "–".into()))
                .child(chip("SPF", "–".into()))
                .child(chip("DMARC", "–".into()))
                .child(div().text_color(theme.muted_foreground).child(text))
            }
            Verification::Checked {
                verdict,
                dkim,
                spf,
                dmarc,
                explanation,
            } => {
                let (icon, color, text) = match verdict {
                    Verdict::Verified => (IconName::ShieldCheck, theme.success, "Verified sender"),
                    Verdict::Unverified => (
                        IconName::ShieldCheck,
                        theme.muted_foreground,
                        "Not verified",
                    ),
                    Verdict::Suspicious => {
                        (IconName::ShieldAlert, theme.danger, "Suspicious sender")
                    }
                };
                let explanation = explanation.clone();
                row.child(Icon::new(icon).xsmall().text_color(color))
                    .child(chip("DKIM", dkim.clone()))
                    .child(chip("SPF", spf.clone()))
                    .child(chip("DMARC", dmarc.clone()))
                    .child(div().text_color(color).child(text))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(explanation.clone())
                            .build(window, cx)
                    })
            }
        }
    }

    fn render_attachments(doc: &MessageDocument, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .flex_none()
            .flex_wrap()
            .gap_2()
            .px_4()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .children(doc.attachments.iter().map(|attachment| {
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(theme.muted)
                    .text_xs()
                    .child(Icon::new(IconName::Paperclip).xsmall())
                    .child(attachment.name.expose().clone())
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .child(format_size(attachment.size)),
                    )
            }))
    }

    fn render_empty(&self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        if let Some(count) = self.selection {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::Mails).large())
                .child(
                    div()
                        .text_color(theme.foreground)
                        .child(format!("{} selected", format_thousands(count as u64))),
                )
                .child("a archives them, ⇧M moves them, m marks them read, s stars them.");
        }
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(Icon::new(IconName::Mail).large())
            .child(
                self.error
                    .clone()
                    .unwrap_or_else(|| SharedString::from("No message selected")),
            )
    }

    /// The body, with the link preview over its bottom left.
    fn render_body(&self, doc: &MessageDocument, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let body_id = SharedString::from(format!("message-body-{}", doc.message));
        let links: Arc<[BodyLink]> = doc.links.expose().as_slice().into();
        let on_click = {
            let links = links.clone();
            move |href: &str, window: &mut Window, cx: &mut App| {
                open_link(href, &links, window, cx);
            }
        };
        let body = match &doc.body {
            MessageBody::Html(html) => self
                .probe
                .attach(TextView::html(body_id, html.expose().clone()), on_click)
                .scrollable(true)
                .selectable(true)
                .size_full()
                .px_4()
                .py_3()
                .text_sm()
                .into_any_element(),
            MessageBody::Markdown(markdown) => self
                .probe
                .attach(
                    TextView::markdown(body_id, markdown.expose().clone()),
                    on_click,
                )
                .scrollable(true)
                .selectable(true)
                .size_full()
                .px_4()
                .py_3()
                .text_sm()
                .into_any_element(),
            MessageBody::PlainText(text) => div()
                .id(body_id)
                .size_full()
                .overflow_y_scroll()
                .px_4()
                .py_3()
                .text_sm()
                .child(Self::render_plain_text(text.expose().clone(), links, cx))
                .into_any_element(),
        };
        let probe = self.probe.clone();
        div()
            .id("message-body")
            .debug_selector(|| "message-body".to_owned())
            .relative()
            .flex_1()
            .min_h_0()
            .on_mouse_move(cx.listener(Self::on_body_mouse_move))
            // The probe's replayed mouse-up stops here: nothing outside the body sees it.
            .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                if probe.is_probing() {
                    cx.stop_propagation();
                }
            })
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered {
                    this.probed_at = None;
                    this.hover_link(None, cx);
                }
            }))
            // Scrolling moves the text under the pointer: the next move looks again.
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                this.probed_at = None;
                this.hover_link(None, cx);
            }))
            .child(body)
            .when_some(self.hovered.clone(), |this, link| {
                this.child(LinkPreview::new(link))
            })
    }
}

impl Focusable for MessageView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for MessageView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let root = v_flex()
            .id("message-view")
            .debug_selector(|| "message-view".to_owned())
            .size_full()
            .key_context("MessageView")
            .track_focus(&self.focus)
            // Clicking into the message gives it focus, so n/p/f/b keep working.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.focus, cx)),
            )
            .bg(theme.background);
        let Some(doc) = self.document.clone() else {
            return root.child(self.render_empty(cx)).into_any_element();
        };
        let sender = doc.from.expose().email.clone();
        let builder = self.context_menu.clone();
        root.child(self.render_header(&doc, cx))
            .when(!doc.attachments.is_empty(), |this| {
                this.child(Self::render_attachments(&doc, cx))
            })
            .child(self.render_body(&doc, cx))
            .context_menu(move |menu, window, cx| {
                let menu = match &builder {
                    Some(builder) => builder(menu, window, cx),
                    None => menu,
                };
                let menu = if menu.is_empty() {
                    menu
                } else {
                    menu.separator()
                };
                let sender = sender.clone();
                menu.item(
                    PopupMenuItem::new("Copy Sender Address").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(sender.clone()));
                    }),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext as _, Entity, Modifiers, TestAppContext, VisualTestContext, point};
    use mazemail_types::{MessageId, Sensitive, ThreadId};
    use time::macros::datetime;

    use super::*;
    use crate::source::Deception;

    const EVIL: &str = "https://evil.example/login?session=1";

    fn document(body: MessageBody, links: Vec<BodyLink>) -> MessageDocument {
        MessageDocument {
            thread: ThreadId(1),
            message: MessageId(1),
            subject: Sensitive::new("Your account".into()),
            from: Sensitive::new(Address::new(Some("PayPal"), "service@paypal.example")),
            to: Sensitive::new(vec![Address::new(None::<String>, "me@example.com")]),
            cc: Sensitive::new(Vec::new()),
            date: datetime!(2026-10-22 10:12 UTC),
            body,
            attachments: Vec::new(),
            verification: Verification::Unavailable,
            client: Sensitive::new(None),
            links: Sensitive::new(links),
        }
    }

    fn open(
        cx: &mut TestAppContext,
        doc: MessageDocument,
    ) -> (Entity<MessageView>, &mut VisualTestContext) {
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let (view, cx) = cx.add_window_view(|_, cx| MessageView::new(UtcOffset::UTC, cx));
        view.update(cx, |view, cx| view.show(Some(doc), cx));
        cx.run_until_parked();
        (view, cx)
    }

    fn hovered(view: &Entity<MessageView>, cx: &mut VisualTestContext) -> Option<HoveredLink> {
        view.read_with(cx, |view, _| view.hovered_link().cloned())
    }

    /// The first characters of the body, inside its padding.
    fn start_of_body(cx: &mut VisualTestContext) -> Point<Pixels> {
        let body = cx.debug_bounds("message-body").unwrap();
        body.origin + point(px(16. + 8.), px(12. + 6.))
    }

    fn deceptive_link() -> Vec<BodyLink> {
        vec![BodyLink {
            text: "www.paypal.com".into(),
            href: EVIL.into(),
            deception: Some(Deception {
                shown: "paypal.com".into(),
                opens: "evil.example".into(),
            }),
        }]
    }

    #[gpui::test]
    fn hovering_an_html_link_shows_its_target_and_leaving_hides_it(cx: &mut TestAppContext) {
        let html = format!("<p><a href=\"{EVIL}\">www.paypal.com</a> to keep your account.</p>");
        let doc = document(
            MessageBody::Html(Sensitive::new(html.into())),
            deceptive_link(),
        );
        let (view, cx) = open(cx, doc);
        assert_eq!(hovered(&view, cx), None);
        assert!(cx.debug_bounds("link-preview").is_none());

        let over_link = start_of_body(cx);
        cx.simulate_mouse_move(over_link, None, Modifiers::none());
        cx.run_until_parked();
        let link = hovered(&view, cx).expect("the link under the pointer");
        assert_eq!(link.target().as_ref(), EVIL);
        assert_eq!(
            link.warning().map(|w| w.as_ref()),
            Some("This link shows paypal.com but opens evil.example.")
        );
        assert!(cx.debug_bounds("link-preview").is_some());
        assert!(cx.debug_bounds("link-preview-warning").is_some());
        // Finding the link doesn't open it.
        assert_eq!(cx.opened_url(), None);

        // Off the link, but still in the body.
        let body = cx.debug_bounds("message-body").unwrap();
        let empty = body.origin + point(body.size.width - px(8.), body.size.height - px(8.));
        cx.simulate_mouse_move(empty, None, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(hovered(&view, cx), None);
        assert!(cx.debug_bounds("link-preview").is_none());

        // Back on the link, then out of the body (into the header).
        cx.simulate_mouse_move(over_link, None, Modifiers::none());
        cx.run_until_parked();
        assert!(hovered(&view, cx).is_some());
        cx.simulate_mouse_move(
            body.origin - point(px(0.), px(20.)),
            None,
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert_eq!(hovered(&view, cx), None);

        // A click on the deceptive link asks first; "Cancel" opens nothing.
        cx.simulate_click(over_link, Modifiers::none());
        cx.run_until_parked();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(cx.opened_url(), None);
        cx.simulate_click(over_link, Modifiers::none());
        cx.run_until_parked();
        cx.simulate_prompt_answer("Open Anyway");
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some(EVIL));
    }

    /// A click on a link opens it normalized, the way the system takes it; cmd-click
    /// and the middle button too, the right button doesn't.
    #[gpui::test]
    fn clicking_a_link_opens_it_normalized(cx: &mut TestAppContext) {
        let html = "<p><a href=\" https://example.com/a b|c?q=ü \">the report</a> is ready.</p>";
        let doc = document(MessageBody::Html(Sensitive::new(html.into())), Vec::new());
        let (_, cx) = open(cx, doc);
        let over_link = start_of_body(cx);
        let normalized = "https://example.com/a%20b%7Cc?q=%C3%BC";

        // The right button opens the context menu, not the link.
        cx.simulate_mouse_down(over_link, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(over_link, MouseButton::Right, Modifiers::none());
        assert_eq!(cx.opened_url(), None);
        cx.simulate_keystrokes("escape");

        cx.simulate_click(over_link, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(cx.opened_url().as_deref(), Some(normalized));
        cx.simulate_click(over_link, Modifiers::command());
        assert_eq!(cx.opened_url().as_deref(), Some(normalized));
        cx.simulate_mouse_down(over_link, MouseButton::Middle, Modifiers::none());
        cx.simulate_mouse_up(over_link, MouseButton::Middle, Modifiers::none());
        assert_eq!(cx.opened_url().as_deref(), Some(normalized));
    }

    /// `mailto:` links ask for a draft; other schemes are refused with a notification.
    #[gpui::test]
    fn mailto_links_compose_and_other_schemes_are_refused(cx: &mut TestAppContext) {
        use gpui_kit::component::WindowExt as _;

        use crate::actions::ComposeTo;
        use crate::links::MailTo;

        struct Host {
            view: Entity<MessageView>,
            composed: Rc<std::cell::RefCell<Vec<MailTo>>>,
        }
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
                let composed = self.composed.clone();
                div()
                    .size_full()
                    .on_action(move |action: &ComposeTo, _, _| {
                        composed.borrow_mut().push(action.0.clone());
                    })
                    .child(self.view.clone())
            }
        }
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let composed = Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = composed.clone();
        // Notifications need a window opened the way the app opens them.
        let (window, host) = cx
            .update(|cx| {
                gpui_kit::open_window(gpui::WindowOptions::default(), cx, |_, cx| {
                    cx.new(|cx| Host {
                        view: cx.new(|cx| MessageView::new(UtcOffset::UTC, cx)),
                        composed: seen,
                    })
                })
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window, cx);
        cx.run_until_parked();
        let view = host.read_with(cx, |host, _| host.view.clone());
        let show = |href: &str, cx: &mut VisualTestContext| {
            let html = format!("<p><a href=\"{href}\">Write to us</a> any time.</p>");
            let doc = document(MessageBody::Html(Sensitive::new(html.into())), Vec::new());
            view.update(cx, |view, cx| view.show(Some(doc), cx));
            cx.run_until_parked();
        };

        show("mailto:help@example.com?subject=Help", cx);
        let over_link = start_of_body(cx);
        cx.simulate_click(over_link, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            *composed.borrow(),
            [MailTo {
                to: vec!["help@example.com".into()],
                subject: Some("Help".into()),
                ..MailTo::default()
            }]
        );
        assert_eq!(cx.opened_url(), None);

        show("javascript:alert(1)", cx);
        cx.simulate_click(over_link, Modifiers::none());
        cx.run_until_parked();
        assert_eq!(cx.opened_url(), None);
        assert_eq!(cx.update(|window, cx| window.notifications(cx).len()), 1);
    }

    #[gpui::test]
    fn hovering_a_markdown_link_shows_its_target(cx: &mut TestAppContext) {
        let markdown = "[Write to us](mailto:help@example.com?subject=Help) any time.";
        let doc = document(
            MessageBody::Markdown(Sensitive::new(markdown.into())),
            Vec::new(),
        );
        let (view, cx) = open(cx, doc);
        let over_link = start_of_body(cx);
        cx.simulate_mouse_move(over_link, None, Modifiers::none());
        cx.run_until_parked();
        let link = hovered(&view, cx).expect("the link under the pointer");
        assert_eq!(link.target().as_ref(), "help@example.com");
        assert_eq!(link.warning(), None);
        assert!(cx.debug_bounds("link-preview").is_some());
        assert!(cx.debug_bounds("link-preview-warning").is_none());
    }

    #[gpui::test]
    fn hovering_a_link_in_plain_text_shows_its_target(cx: &mut TestAppContext) {
        let text = "https://example.com/reset is where you reset it.";
        let doc = document(
            MessageBody::PlainText(Sensitive::new(text.into())),
            Vec::new(),
        );
        let (view, cx) = open(cx, doc);
        let over_link = start_of_body(cx);
        cx.simulate_mouse_move(over_link, None, Modifiers::none());
        cx.run_until_parked();
        let link = hovered(&view, cx).expect("the link under the pointer");
        assert_eq!(link.target().as_ref(), "https://example.com/reset");
        assert!(cx.debug_bounds("link-preview").is_some());

        let body = cx.debug_bounds("message-body").unwrap();
        cx.simulate_mouse_move(
            body.origin - point(px(0.), px(20.)),
            None,
            Modifiers::none(),
        );
        cx.run_until_parked();
        assert_eq!(hovered(&view, cx), None);
    }

    /// The probe's replayed mouse-up stays inside the body: what's around the message
    /// view only sees real ones.
    #[gpui::test]
    fn probing_for_links_is_invisible_outside_the_body(cx: &mut TestAppContext) {
        struct Host {
            view: Entity<MessageView>,
            mouse_ups: Rc<std::cell::Cell<usize>>,
        }
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<'_, Self>) -> impl IntoElement {
                let mouse_ups = self.mouse_ups.clone();
                div()
                    .size_full()
                    .on_mouse_up(MouseButton::Left, move |_, _, _| {
                        mouse_ups.set(mouse_ups.get() + 1);
                    })
                    .child(self.view.clone())
            }
        }
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let mouse_ups = Rc::new(std::cell::Cell::new(0));
        let ups = mouse_ups.clone();
        let (host, cx) = cx.add_window_view(|_, cx| Host {
            view: cx.new(|cx| MessageView::new(UtcOffset::UTC, cx)),
            mouse_ups: ups,
        });
        let view = host.read_with(cx, |host, _| host.view.clone());
        let html = format!("<p><a href=\"{EVIL}\">www.paypal.com</a> and more text.</p>");
        let doc = document(
            MessageBody::Html(Sensitive::new(html.into())),
            deceptive_link(),
        );
        view.update(cx, |view, cx| view.show(Some(doc), cx));
        cx.run_until_parked();

        let over_link = start_of_body(cx);
        let body = cx.debug_bounds("message-body").unwrap();
        let empty = body.origin + point(body.size.width - px(8.), body.size.height - px(8.));
        for position in [over_link, empty, over_link] {
            cx.simulate_mouse_move(position, None, Modifiers::none());
            cx.run_until_parked();
        }
        assert!(hovered(&view, cx).is_some());
        assert_eq!(mouse_ups.get(), 0);
        cx.simulate_click(empty, Modifiers::none());
        assert_eq!(mouse_ups.get(), 1);
    }

    #[gpui::test]
    fn showing_another_message_hides_the_preview(cx: &mut TestAppContext) {
        let doc = document(
            MessageBody::Html(Sensitive::new("<p>Hi</p>".into())),
            deceptive_link(),
        );
        let (view, cx) = open(cx, doc.clone());
        view.update(cx, |view, cx| view.hover_link(Some(EVIL), cx));
        assert!(hovered(&view, cx).is_some_and(|link| link.warning().is_some()));
        view.update(cx, |view, cx| view.show(Some(doc), cx));
        assert_eq!(hovered(&view, cx), None);
    }
}

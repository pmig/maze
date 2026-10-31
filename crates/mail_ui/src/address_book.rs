//! The Address Book space (CONCEPT §8.2, PLAN-contacts §1, ADR-024): the second
//! entry of the spaces bar.
//!
//! ```text
//! spaces | book pane | contact list | contact | agent
//! ```
//!
//! - **Book pane** (in the folder pane's place, `cmd-b` hides it too): All Contacts
//!   with the number of addresses, then each account's books (Google's My Contacts
//!   and Other Contacts, …), then Collected (addresses you wrote to) and imported
//!   address books, then books of removed accounts under "Other".
//! - **Contact list**: one row per address, with the avatar, name, address and a label
//!   per account (or collected or imported book) the address is in, so the same
//!   person in two accounts is one row with two labels. The Quick Filter (`cmd-f`)
//!   narrows it by name or address; the button next to the count sorts by name or by
//!   when you last wrote to them. Up and Down move through it.
//! - **Detail pane**: the avatar, the name, every address and phone number (each with
//!   a copy button), New Mail (a new message with the contact as a recipient chip),
//!   Show Mail (the mail from and to the address, in the Mail space), and each book's
//!   version with its label. Editing writes to the account's server (ADR-024), so the
//!   Edit button only shows on versions whose book the backend can write to
//!   ([`MailSource::contact_book_editable`]); it hands a [`ContactEdit`] to whoever
//!   listens ([`AddressBookEvent::Edit`]), which then saves it with
//!   [`MailSource::save_contact`]. No book is editable yet.
//!
//! The selected book and the order are remembered in `ui_state.json`. The list reads
//! every row of a book at once (off the main thread) and draws only the visible ones,
//! which keeps 10,000 contacts fast.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Hsla, InteractiveElement as _, IntoElement, ParentElement, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement as _, Styled, Subscription, Task,
    UniformListScrollHandle, Window, div, hsla, prelude::FluentBuilder as _, px, uniform_list,
};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    v_flex,
};
use mazemail_types::{Address, PRODUCT_NAME};
use time::UtcOffset;
use ui::assets::IconName;
use ui::components::{Avatar, CountBadge, initials};
use ui::keymap::shortcut_label;
use ui::theme::scaled;

use crate::format::{DateFormatter, format_thousands};
use crate::layout::LayoutPrefs;
use crate::source::{
    AddressBookEntry, BookKind, ContactDetail, ContactEdit, ContactLabel, ContactPhone,
    ContactQuery, ContactRow, ContactSort, ContactVersion, MailSource, SourceError,
};
use crate::ui_state::{AddressBookState, UiStateStore};

/// How long after the last key the Quick Filter applies.
const FILTER_DEBOUNCE: Duration = Duration::from_millis(120);

/// What the empty Address Book says.
pub const NO_CONTACTS: &str = "Addresses you write to appear here, and address books you \
     import from Thunderbird. Google and Microsoft contacts come in a later version.";

/// What the Address Book asks the window for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressBookEvent {
    /// New Mail: a new message to this address.
    NewMail(Address),
    /// Show Mail: the mail from and to this address, in the Mail space.
    ShowMail(SharedString),
    /// Edit on an editable version: the contact editor starts from this.
    Edit(ContactEdit),
}

/// The Address Book space: book pane, contact list and detail pane. See the
/// [module docs](self).
pub struct AddressBook {
    source: Arc<dyn MailSource>,
    ui_state: UiStateStore,
    prefs: Rc<RefCell<LayoutPrefs>>,
    local_offset: UtcOffset,
    focus: FocusHandle,
    filter: Entity<InputState>,
    books: Vec<AddressBookEntry>,
    /// Addresses across all books (All Contacts' count).
    all_count: usize,
    /// The selected book; All Contacts when `None`.
    book: Option<SharedString>,
    sort: ContactSort,
    rows: Arc<[ContactRow]>,
    selected: Option<usize>,
    detail: Option<ContactDetail>,
    scroll: UniformListScrollHandle,
    loading: Option<Task<()>>,
    /// The first load finished (no empty state flashes before it).
    loaded: bool,
    _subscriptions: Vec<Subscription>,
}

impl std::fmt::Debug for AddressBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddressBook")
            .field("rows", &self.rows.len())
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<AddressBookEvent> for AddressBook {}

impl Focusable for AddressBook {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The color of an account's labels and dot: one of eight hues picked by the
/// account's color key, readable in light and dark themes.
pub fn label_color(key: u32, dark: bool) -> Hsla {
    const HUES: [f32; 8] = [0.58, 0.38, 0.08, 0.78, 0.95, 0.5, 0.13, 0.68];
    let hue = HUES[(key as usize) % HUES.len()];
    if dark {
        hsla(hue, 0.6, 0.72, 1.)
    } else {
        hsla(hue, 0.65, 0.38, 1.)
    }
}

/// A label chip: the account's name in its color, or a neutral one.
fn label_chip(label: &ContactLabel, cx: &App) -> impl IntoElement + use<> {
    let theme = cx.theme();
    let (bg, fg) = match label.color {
        Some(key) => {
            let color = label_color(key, theme.is_dark());
            (color.opacity(0.14), color)
        }
        None => (theme.muted, theme.muted_foreground),
    };
    div()
        .flex_none()
        .max_w(scaled(140., cx))
        .px(scaled(6., cx))
        .rounded(theme.radius)
        .bg(bg)
        .text_color(fg)
        .text_xs()
        .truncate()
        .child(label.text.clone())
}

impl AddressBook {
    /// The Address Book over `source`, with the remembered book and order.
    pub fn new(
        source: Arc<dyn MailSource>,
        ui_state: UiStateStore,
        prefs: Rc<RefCell<LayoutPrefs>>,
        local_offset: UtcOffset,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let saved = ui_state.address_book();
        let filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(format!("Quick filter…  {}", shortcut_label("cmd-f", cx)))
                .clean_on_escape()
        });
        let subscriptions = vec![cx.subscribe_in(&filter, window, Self::on_filter_event)];
        let mut this = Self {
            source,
            ui_state,
            prefs,
            local_offset,
            focus: cx.focus_handle(),
            filter,
            books: Vec::new(),
            all_count: 0,
            book: saved.book.map(SharedString::from),
            sort: saved.sort,
            rows: Arc::from(Vec::new()),
            selected: None,
            detail: None,
            scroll: UniformListScrollHandle::new(),
            loading: None,
            loaded: false,
            _subscriptions: subscriptions,
        };
        this.reload(cx);
        this
    }

    // ---- state -------------------------------------------------------------------

    /// The books of the book pane.
    pub fn books(&self) -> &[AddressBookEntry] {
        &self.books
    }

    /// The selected book; `None` is All Contacts.
    pub fn selected_book(&self) -> Option<&SharedString> {
        self.book.as_ref()
    }

    /// The contact list's order.
    pub fn sort(&self) -> ContactSort {
        self.sort
    }

    /// The rows of the contact list.
    pub fn rows(&self) -> &Arc<[ContactRow]> {
        &self.rows
    }

    /// The selected row's index.
    pub fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// The selected row.
    pub fn selected_row(&self) -> Option<&ContactRow> {
        self.selected.and_then(|ix| self.rows.get(ix))
    }

    /// The contact shown in the detail pane.
    pub fn detail(&self) -> Option<&ContactDetail> {
        self.detail.as_ref()
    }

    /// Whether the list is being read.
    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    /// The Quick Filter.
    pub fn filter(&self) -> &Entity<InputState> {
        &self.filter
    }

    /// Whether the Quick Filter has the focus.
    pub fn filter_focused(&self, window: &Window, cx: &App) -> bool {
        self.filter.focus_handle(cx).is_focused(window)
    }

    fn query(&self, cx: &App) -> ContactQuery {
        ContactQuery {
            book: self.book.clone(),
            text: self.filter.read(cx).value().trim().to_owned(),
            sort: self.sort,
        }
    }

    fn remember(&self) {
        self.ui_state.set_address_book(AddressBookState {
            book: self.book.as_ref().map(ToString::to_string),
            sort: self.sort,
        });
    }

    // ---- loading -----------------------------------------------------------------

    /// Re-reads the books and the list (after contacts changed), keeping the selected
    /// contact selected.
    pub fn reload(&mut self, cx: &mut Context<'_, Self>) {
        let source = self.source.clone();
        let query = self.query(cx);
        let keep = self.selected_row().map(|row| row.email.clone());
        self.loading = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let books = source.address_books();
                    let rows = source.contacts(&query);
                    let unfiltered = query.book.is_none() && query.text.is_empty();
                    let all_count = match &rows {
                        Ok(rows) if unfiltered => rows.len(),
                        _ => source
                            .contacts(&ContactQuery::default())
                            .map_or(0, |all| all.len()),
                    };
                    (books, all_count, rows)
                })
                .await;
            this.update(cx, |this, cx| this.loaded(loaded, keep, cx))
                .ok();
        }));
    }

    fn loaded(
        &mut self,
        (books, all_count, rows): (
            Vec<AddressBookEntry>,
            usize,
            Result<Arc<[ContactRow]>, SourceError>,
        ),
        keep: Option<SharedString>,
        cx: &mut Context<'_, Self>,
    ) {
        self.loading = None;
        self.loaded = true;
        self.books = books;
        self.all_count = all_count;
        // The book went away (its account was removed): back to All Contacts.
        if let Some(book) = &self.book
            && !self.books.iter().any(|b| &b.id == book)
            && !self.books.is_empty()
        {
            self.book = None;
            self.remember();
            self.reload(cx);
            return;
        }
        self.rows = rows.unwrap_or_else(|error| {
            tracing::warn!(%error, "cannot read the contact list");
            Arc::from(Vec::new())
        });
        let ix = keep
            .and_then(|email| {
                self.rows
                    .iter()
                    .position(|row| row.email.eq_ignore_ascii_case(&email))
            })
            .or((!self.rows.is_empty()).then_some(0));
        self.show(ix, cx);
    }

    /// Selects row `ix` (or nothing) and shows its contact.
    fn show(&mut self, ix: Option<usize>, cx: &mut Context<'_, Self>) {
        self.selected = ix.filter(|ix| *ix < self.rows.len());
        self.detail = self.selected_row().and_then(|row| {
            self.source.contact(&row.email).unwrap_or_else(|error| {
                tracing::warn!(%error, "cannot read a contact");
                None
            })
        });
        if let Some(ix) = self.selected {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
        cx.notify();
    }

    // ---- actions -----------------------------------------------------------------

    /// Shows a book (`None`: All Contacts) and remembers it.
    pub fn select_book(&mut self, book: Option<SharedString>, cx: &mut Context<'_, Self>) {
        if self.book == book {
            return;
        }
        self.book = book;
        self.remember();
        self.reload(cx);
        cx.notify();
    }

    /// Sorts the list and remembers the order.
    pub fn set_sort(&mut self, sort: ContactSort, cx: &mut Context<'_, Self>) {
        if self.sort == sort {
            return;
        }
        self.sort = sort;
        self.remember();
        self.reload(cx);
        cx.notify();
    }

    /// Selects row `ix`.
    pub fn select(&mut self, ix: usize, cx: &mut Context<'_, Self>) {
        if self.selected != Some(ix) {
            self.show(Some(ix), cx);
        }
    }

    /// Moves the selection by `delta` rows, stopping at the ends.
    pub fn step(&mut self, delta: isize, cx: &mut Context<'_, Self>) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let ix = match self.selected {
            Some(ix) => ix.saturating_add_signed(delta).min(last),
            None => 0,
        };
        self.select(ix, cx);
    }

    /// Selects the first (`false`) or last (`true`) row.
    pub fn edge(&mut self, last: bool, cx: &mut Context<'_, Self>) {
        if !self.rows.is_empty() {
            self.select(if last { self.rows.len() - 1 } else { 0 }, cx);
        }
    }

    /// Focuses the list.
    pub fn focus_list(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    /// Focuses the Quick Filter (`cmd-f`).
    pub fn focus_filter(&self, window: &mut Window, cx: &mut App) {
        let handle = self.filter.focus_handle(cx);
        window.focus(&handle, cx);
    }

    /// New Mail to the shown contact.
    pub fn new_mail(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(detail) = &self.detail {
            let address = Address::new(
                detail.name.as_ref().map(ToString::to_string),
                detail.email.to_string(),
            );
            cx.emit(AddressBookEvent::NewMail(address));
        }
    }

    /// Show Mail of the shown contact.
    pub fn show_mail(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(detail) = &self.detail {
            cx.emit(AddressBookEvent::ShowMail(detail.email.clone()));
        }
    }

    fn edit(&mut self, version: &ContactVersion, cx: &mut Context<'_, Self>) {
        cx.emit(AddressBookEvent::Edit(ContactEdit::of(version)));
    }

    fn on_filter_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            InputEvent::Change => {
                self.loading = Some(cx.spawn_in(window, async move |this, cx| {
                    cx.background_executor().timer(FILTER_DEBOUNCE).await;
                    this.update(cx, |this, cx| this.reload(cx)).ok();
                }));
            }
            InputEvent::PressEnter { .. } => self.focus_list(window, cx),
            InputEvent::Focus | InputEvent::Blur => {}
        }
    }

    // ---- book pane ---------------------------------------------------------------

    /// The book pane, drawn by the window in the folder pane's column.
    pub fn render_book_pane(&mut self, cx: &mut Context<'_, Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let dark = cx.theme().is_dark();
        let section = |label: &'static str| {
            div()
                .px_2()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(label)
        };
        let mut pane = v_flex()
            .id("book-pane")
            .size_full()
            .overflow_y_scroll()
            .bg(theme.sidebar)
            .pt_1()
            .px_1()
            .pb_2()
            .child(self.book_row(
                None,
                IconName::Users,
                "All Contacts".into(),
                self.all_count as u64,
                0.,
                cx,
            ));
        let accounts: Vec<&AddressBookEntry> = self
            .books
            .iter()
            .filter(|b| matches!(b.kind, BookKind::Account { .. }))
            .collect();
        if !accounts.is_empty() {
            pane = pane.child(section("ACCOUNTS"));
            let mut last_account = None;
            for book in accounts {
                let BookKind::Account { account, name } = &book.kind else {
                    continue;
                };
                if last_account != Some(*account) {
                    last_account = Some(*account);
                    pane =
                        pane.child(
                            h_flex()
                                .px_2()
                                .py_0p5()
                                .gap_1p5()
                                .text_sm()
                                .font_semibold()
                                .child(div().size(scaled(8., cx)).flex_none().rounded_full().bg(
                                    book.color.map_or(theme.muted_foreground, |key| {
                                        label_color(key, dark)
                                    }),
                                ))
                                .child(div().truncate().child(name.clone())),
                        );
                }
                pane = pane.child(self.book_row(
                    Some(book.id.clone()),
                    IconName::BookUser,
                    book.name.clone(),
                    book.count,
                    14.,
                    cx,
                ));
            }
        }
        let local: Vec<&AddressBookEntry> = self
            .books
            .iter()
            .filter(|b| matches!(b.kind, BookKind::Collected | BookKind::Imported))
            .collect();
        if !local.is_empty() {
            pane = pane.child(section("ON THIS COMPUTER"));
            for book in local {
                let icon = if book.kind == BookKind::Collected {
                    IconName::Send
                } else {
                    IconName::Book
                };
                pane = pane.child(self.book_row(
                    Some(book.id.clone()),
                    icon,
                    book.name.clone(),
                    book.count,
                    0.,
                    cx,
                ));
            }
        }
        let other: Vec<&AddressBookEntry> = self
            .books
            .iter()
            .filter(|b| b.kind == BookKind::Other)
            .collect();
        if !other.is_empty() {
            pane = pane.child(section("OTHER"));
            for book in other {
                pane = pane.child(self.book_row(
                    Some(book.id.clone()),
                    IconName::BookUser,
                    book.name.clone(),
                    book.count,
                    0.,
                    cx,
                ));
            }
        }
        pane.into_any_element()
    }

    fn book_row(
        &self,
        book: Option<SharedString>,
        icon: IconName,
        name: SharedString,
        count: u64,
        indent: f32,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let selected = self.book == book;
        let key: SharedString = book.clone().unwrap_or_else(|| "all".into());
        h_flex()
            .id(SharedString::from(format!("book-row:{key}")))
            .debug_selector(move || format!("book-row:{key}"))
            .px_2()
            .py_0p5()
            .pl(scaled(8. + indent, cx))
            .gap_1p5()
            .rounded(cx.theme().radius)
            .text_sm()
            .cursor_pointer()
            .map(|this| {
                if selected {
                    this.bg(theme.list_active)
                } else {
                    this.hover(|this| this.bg(theme.list_hover))
                }
            })
            .child(Icon::new(icon).small().text_color(theme.muted_foreground))
            .child(div().flex_1().min_w_0().truncate().child(name))
            .child(CountBadge::new(count).emphasized(selected))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.select_book(book.clone(), cx);
            }))
    }

    // ---- contact list ------------------------------------------------------------

    fn title(&self) -> SharedString {
        match &self.book {
            None => "All Contacts".into(),
            Some(id) => self.books.iter().find(|b| &b.id == id).map_or_else(
                || id.clone(),
                |book| match &book.kind {
                    BookKind::Account { name, .. } => format!("{} · {name}", book.name).into(),
                    _ => book.name.clone(),
                },
            ),
        }
    }

    fn render_list_header(&self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let count = self.rows.len() as u64;
        let noun = if count == 1 { "contact" } else { "contacts" };
        let (icon, tooltip, next) = match self.sort {
            ContactSort::Name => (
                IconName::ArrowDownAZ,
                "Sorted by name: click to sort by last used",
                ContactSort::LastUsed,
            ),
            ContactSort::LastUsed => (
                IconName::Clock,
                "Sorted by last used: click to sort by name",
                ContactSort::Name,
            ),
        };
        v_flex()
            .flex_none()
            .gap_1p5()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_1()
                    .px_1()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_semibold()
                            .child(self.title()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .mr_1()
                            .text_color(theme.muted_foreground)
                            .child(format!("{} {noun}", format_thousands(count))),
                    )
                    .child(
                        div().debug_selector(|| "contact-sort".into()).child(
                            Button::new("contact-sort")
                                .ghost()
                                .xsmall()
                                .icon(icon)
                                .tooltip(tooltip)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.set_sort(next, cx);
                                })),
                        ),
                    ),
            )
            .child(
                div().key_context("QuickFilter").child(
                    Input::new(&self.filter).small().cleanable(true).prefix(
                        Icon::new(IconName::ListFilter)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    ),
                ),
            )
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<'_, Self>) -> Vec<AnyElement> {
        let theme = cx.theme().colors;
        let rows = self.rows.clone();
        let dates = DateFormatter::new(self.local_offset);
        let avatar = scaled(30., cx);
        range
            .filter_map(|ix| {
                let row = rows.get(ix)?;
                let selected = self.selected == Some(ix);
                let name = row.name.clone().unwrap_or_else(|| row.email.clone());
                let last_used = (self.sort == ContactSort::LastUsed)
                    .then(|| row.last_used.map(|at| dates.short(at)))
                    .flatten();
                Some(
                    h_flex()
                        .id(("contact-row", ix))
                        .debug_selector(move || format!("contact-row-{ix}"))
                        .h(scaled(52., cx))
                        .w_full()
                        .px_3()
                        .gap_2p5()
                        .relative()
                        .border_b_1()
                        .border_color(theme.border.opacity(0.5))
                        .map(|this| {
                            if selected {
                                this.bg(theme.list_active).child(
                                    div()
                                        .absolute()
                                        .left_0()
                                        .top_0()
                                        .bottom_0()
                                        .w(px(2.))
                                        .bg(theme.primary),
                                )
                            } else {
                                this.hover(|this| this.bg(theme.list_hover))
                            }
                        })
                        .child(Avatar::new(
                            initials(row.name.as_deref(), &row.email),
                            avatar,
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_sm()
                                                .font_medium()
                                                .child(name),
                                        )
                                        .when_some(last_used, |this, date| {
                                            this.child(
                                                div()
                                                    .flex_none()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(date),
                                            )
                                        }),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(row.email.clone()),
                                        )
                                        .children(
                                            row.labels.iter().map(|label| label_chip(label, cx)),
                                        ),
                                ),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.select(ix, cx);
                            this.focus_list(window, cx);
                        }))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_list(&mut self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let filter_text = self.filter.read(cx).value();
        let empty = self.loaded && self.rows.is_empty() && self.loading.is_none();
        let body = if empty {
            let (title, detail): (SharedString, SharedString) = if self.books.is_empty() {
                ("No contacts yet".into(), NO_CONTACTS.into())
            } else if !filter_text.trim().is_empty() {
                (
                    "No matching contacts".into(),
                    format!("Nothing here matches “{}”.", filter_text.trim()).into(),
                )
            } else {
                ("No contacts in this book".into(), SharedString::default())
            };
            v_flex()
                .id("contacts-empty")
                .debug_selector(|| "contacts-empty".into())
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .px_6()
                .text_center()
                .child(
                    Icon::new(IconName::BookUser)
                        .large()
                        .text_color(theme.muted_foreground),
                )
                .child(div().text_sm().font_semibold().child(title))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(detail),
                )
                .into_any_element()
        } else {
            uniform_list(
                "contacts",
                self.rows.len(),
                cx.processor(|this, range: Range<usize>, _, cx| this.render_rows(range, cx)),
            )
            .size_full()
            .track_scroll(&self.scroll)
            .into_any_element()
        };
        let width = self.prefs.borrow().list;
        v_flex()
            .w(px(width))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(self.render_list_header(cx))
            .child(
                div()
                    .id("contact-list")
                    .flex_1()
                    .min_h_0()
                    .key_context("ContactList")
                    .track_focus(&self.focus)
                    .child(body),
            )
    }

    // ---- detail pane -------------------------------------------------------------

    fn copy_row(
        id: SharedString,
        icon: IconName,
        kind: Option<SharedString>,
        value: SharedString,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let copied = value.clone();
        h_flex()
            .gap_2()
            .py_0p5()
            .child(Icon::new(icon).small().text_color(theme.muted_foreground))
            .child(div().min_w_0().truncate().text_sm().child(value))
            .when_some(kind, |this, kind| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(kind),
                )
            })
            .child(
                div()
                    .debug_selector({
                        let id = id.clone();
                        move || id.to_string()
                    })
                    .child(
                        Button::new(id)
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip("Copy")
                            .on_click(move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copied.to_string(),
                                ));
                                window.push_notification(
                                    Notification::info("Copied to the clipboard.")
                                        .title(PRODUCT_NAME),
                                    cx,
                                );
                            }),
                    ),
            )
    }

    fn render_version(
        &self,
        ix: usize,
        version: &ContactVersion,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let dates = DateFormatter::new(self.local_offset);
        let book_name = match version.label.color {
            Some(_) => version.book_name.clone(),
            None if version.label.text == version.book_name => SharedString::default(),
            None => version.book_name.clone(),
        };
        let mut facts: Vec<SharedString> = Vec::new();
        if let Some(name) = &version.name {
            facts.push(name.clone());
        }
        facts.extend(version.emails.iter().cloned());
        for phone in &version.phones {
            facts.push(match &phone.kind {
                Some(kind) => format!("{} ({kind})", phone.number).into(),
                None => phone.number.clone(),
            });
        }
        match (&version.title, &version.organization) {
            (Some(title), Some(org)) => facts.push(format!("{title}, {org}").into()),
            (Some(one), None) | (None, Some(one)) => facts.push(one.clone()),
            (None, None) => {}
        }
        if version.frequency > 0 {
            let times = if version.frequency == 1 {
                "once".to_owned()
            } else {
                format!("{} times", version.frequency)
            };
            facts.push(match version.last_used {
                Some(at) => format!("You wrote {times}, last on {}", dates.long(at)).into(),
                None => format!("You wrote {times}").into(),
            });
        }
        let edit = version.editable.then(|| {
            let version = version.clone();
            Button::new(("contact-edit", ix))
                .ghost()
                .xsmall()
                .icon(IconName::Pencil)
                .label("Edit")
                .on_click(cx.listener(move |this, _, _, cx| this.edit(&version, cx)))
        });
        v_flex()
            .debug_selector(move || format!("contact-version-{ix}"))
            .gap_1()
            .p_3()
            .rounded(cx.theme().radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(label_chip(&version.label, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(book_name),
                    )
                    .children(edit),
            )
            .children(facts.into_iter().map(|fact| {
                div()
                    .text_sm()
                    .text_color(theme.foreground)
                    .truncate()
                    .child(fact)
            }))
    }

    fn render_detail(&mut self, cx: &mut Context<'_, Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().colors;
        let pane = v_flex()
            .id("contact-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background);
        let Some(detail) = self.detail.clone() else {
            let hint: SharedString = if self.loaded && self.books.is_empty() {
                SharedString::default()
            } else {
                "Select a contact".into()
            };
            return pane
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(hint);
        };
        let name = detail.name.clone().unwrap_or_else(|| detail.email.clone());
        let subtitle = match (&detail.title, &detail.organization) {
            (Some(title), Some(org)) => Some(SharedString::from(format!("{title}, {org}"))),
            (Some(one), None) | (None, Some(one)) => Some(one.clone()),
            (None, None) => None,
        };
        let mut labels: Vec<ContactLabel> = Vec::new();
        for version in &detail.versions {
            if !labels.contains(&version.label) {
                labels.push(version.label.clone());
            }
        }
        let section = |label: &'static str| {
            div()
                .pt_3()
                .pb_1()
                .text_xs()
                .font_semibold()
                .text_color(theme.muted_foreground)
                .child(label)
        };
        let emails = detail.emails.iter().enumerate().map(|(ix, email)| {
            Self::copy_row(
                format!("copy-email-{ix}").into(),
                IconName::Mail,
                None,
                email.clone(),
                cx,
            )
            .into_any_element()
        });
        let emails: Vec<AnyElement> = emails.collect();
        let phones: Vec<AnyElement> = detail
            .phones
            .iter()
            .enumerate()
            .map(|(ix, ContactPhone { kind, number })| {
                Self::copy_row(
                    format!("copy-phone-{ix}").into(),
                    IconName::Phone,
                    kind.clone(),
                    number.clone(),
                    cx,
                )
                .into_any_element()
            })
            .collect();
        let versions: Vec<AnyElement> = detail
            .versions
            .iter()
            .enumerate()
            .map(|(ix, version)| self.render_version(ix, version, cx).into_any_element())
            .collect();
        pane.overflow_y_scroll().child(
            v_flex()
                .w_full()
                .max_w(scaled(640., cx))
                .px_6()
                .py_5()
                .gap_1()
                .child(
                    h_flex()
                        .gap_4()
                        .child(Avatar::new(
                            initials(detail.name.as_deref(), &detail.email),
                            scaled(64., cx),
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .debug_selector(|| "contact-name".into())
                                        .text_xl()
                                        .font_semibold()
                                        .truncate()
                                        .child(name),
                                )
                                .when_some(subtitle, |this, subtitle| {
                                    this.child(
                                        div()
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(subtitle),
                                    )
                                })
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .flex_wrap()
                                        .children(labels.iter().map(|l| label_chip(l, cx))),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .pt_3()
                        .gap_2()
                        .child(
                            div().debug_selector(|| "contact-new-mail".into()).child(
                                Button::new("contact-new-mail")
                                    .primary()
                                    .small()
                                    .icon(IconName::MailPlus)
                                    .label("New Mail")
                                    .on_click(cx.listener(|this, _, _, cx| this.new_mail(cx))),
                            ),
                        )
                        .child(
                            div().debug_selector(|| "contact-show-mail".into()).child(
                                Button::new("contact-show-mail")
                                    .outline()
                                    .small()
                                    .icon(IconName::MailSearch)
                                    .label("Show Mail")
                                    .tooltip("Mail from and to this address")
                                    .on_click(cx.listener(|this, _, _, cx| this.show_mail(cx))),
                            ),
                        ),
                )
                .child(section("EMAIL"))
                .children(emails)
                .when(!phones.is_empty(), |this| {
                    this.child(section("PHONE")).children(phones)
                })
                .child(section(if detail.versions.len() == 1 {
                    "ADDRESS BOOK"
                } else {
                    "IN YOUR ADDRESS BOOKS"
                }))
                .child(v_flex().gap_2().children(versions)),
        )
    }
}

impl Render for AddressBook {
    fn render(&mut self, _: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        h_flex()
            .id("address-book")
            .key_context("AddressBook")
            .size_full()
            .child(self.render_list(cx))
            .child(self.render_detail(cx))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use time::macros::datetime;

    use super::*;
    use crate::fake::FakeMailSource;
    use crate::window::{ComposeRequest, MailWindow, MailWindowOptions, Space, show_mail_query};

    fn open_with(
        source: FakeMailSource,
        cx: &mut TestAppContext,
    ) -> (Entity<MailWindow>, &mut VisualTestContext) {
        cx.update(|cx| {
            ui::init(cx);
            crate::init(cx);
        });
        let source: Arc<dyn MailSource> = Arc::new(source);
        let (view, cx) = cx.add_window_view(|window, cx| {
            MailWindow::new(source, &MailWindowOptions::default(), window, cx)
        });
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        (view, cx)
    }

    fn open(cx: &mut TestAppContext) -> (Entity<MailWindow>, &mut VisualTestContext) {
        open_with(
            FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC)),
            cx,
        )
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} is on screen"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn drawn(cx: &mut VisualTestContext, selector: String) -> bool {
        let selector: &'static str = Box::leak(selector.into_boxed_str());
        cx.debug_bounds(selector).is_some()
    }

    fn space(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> Space {
        view.read_with(cx, |w, _| w.space())
    }

    fn book(view: &Entity<MailWindow>, cx: &mut VisualTestContext) -> Entity<AddressBook> {
        view.read_with(cx, |w, _| w.address_book().clone())
    }

    fn emails(book: &Entity<AddressBook>, cx: &mut VisualTestContext) -> Vec<String> {
        book.read_with(cx, |b, _| {
            b.rows().iter().map(|r| r.email.to_string()).collect()
        })
    }

    fn label_texts(row: &ContactRow) -> Vec<&str> {
        row.labels.iter().map(|l| l.text.as_ref()).collect()
    }

    #[gpui::test]
    fn the_spaces_bar_and_the_keys_switch_spaces(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        assert_eq!(space(&view, cx), Space::Mail);
        click(cx, "space-contacts");
        assert_eq!(space(&view, cx), Space::AddressBook);
        assert!(cx.debug_bounds("book-row:all").is_some(), "the book pane");
        assert!(
            cx.debug_bounds("contact-row-0").is_some(),
            "the contact list"
        );
        click(cx, "space-mail");
        assert_eq!(space(&view, cx), Space::Mail);
        assert!(cx.debug_bounds("book-row:all").is_none());

        cx.simulate_keystrokes("cmd-shift-b");
        cx.run_until_parked();
        assert_eq!(space(&view, cx), Space::AddressBook);
        // The list has the focus, so up and down move through it.
        cx.simulate_keystrokes("down down");
        assert_eq!(
            book(&view, cx).read_with(cx, |b, _| b.selected_index()),
            Some(2)
        );
        cx.simulate_keystrokes("up");
        assert_eq!(
            book(&view, cx).read_with(cx, |b, _| b.selected_index()),
            Some(1)
        );
        // The same key goes back to Mail, and comes back again.
        cx.simulate_keystrokes("cmd-shift-b");
        cx.run_until_parked();
        assert_eq!(space(&view, cx), Space::Mail);
        cx.simulate_keystrokes("cmd-shift-b");
        cx.run_until_parked();
        assert_eq!(space(&view, cx), Space::AddressBook);
        // From the palette too.
        cx.dispatch_action(crate::actions::ShowMail);
        assert_eq!(space(&view, cx), Space::Mail);
    }

    #[gpui::test]
    fn the_book_pane_picks_what_the_list_shows(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        cx.simulate_keystrokes("cmd-shift-b");
        cx.run_until_parked();
        let book = book(&view, cx);
        let all = emails(&book, cx);
        assert_eq!(all.len(), 17, "one row per address");
        let names: Vec<String> = book.read_with(cx, |b, _| {
            b.books().iter().map(|b| b.name.to_string()).collect()
        });
        assert_eq!(
            names,
            [
                "My Contacts",
                "Other Contacts",
                "Contacts",
                "Collected",
                "Personal Address Book"
            ]
        );

        click(cx, "book-row:collected");
        let collected = emails(&book, cx);
        assert!(collected.len() < all.len());
        assert!(book.read_with(cx, |b, _| {
            b.rows()
                .iter()
                .all(|r| r.labels.iter().any(|l| l.text.as_ref() == "Collected"))
        }));
        assert_eq!(
            book.read_with(cx, |b, _| b.selected_book().cloned()),
            Some("collected".into())
        );
        // Remembered for the next start.
        let saved = view.read_with(cx, |w, _| w.views_ui_state().address_book());
        assert_eq!(saved.book.as_deref(), Some("collected"));

        click(cx, "book-row:google-other:fake-1");
        assert_eq!(
            emails(&book, cx),
            [
                "clara.novak@acme.example",
                "secretary@club.example",
                "kathrin@club.example"
            ]
        );
        click(cx, "book-row:all");
        assert_eq!(emails(&book, cx), all);
    }

    #[gpui::test]
    fn one_row_per_person_with_a_label_per_account_and_every_version(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        click(cx, "space-contacts");
        let book = book(&view, cx);
        let anna = book.read_with(cx, |b, _| b.rows()[0].clone());
        assert_eq!(anna.name.as_deref(), Some("Anna Example"));
        assert_eq!(
            label_texts(&anna),
            ["Gmail (personal)", "Microsoft 365", "Collected"]
        );
        assert_eq!(anna.labels[0].color, Some(1));
        assert_eq!(anna.labels[2].color, None);
        let detail = book.read_with(cx, |b, _| b.detail().cloned()).unwrap();
        let versions: Vec<&str> = detail
            .versions
            .iter()
            .map(|v| v.book_name.as_ref())
            .collect();
        assert_eq!(versions, ["My Contacts", "Contacts", "Collected"]);
        assert_eq!(detail.phones.len(), 2);
        for ix in 0..3 {
            assert!(
                drawn(cx, format!("contact-version-{ix}")),
                "version {ix} is shown"
            );
        }
        assert!(cx.debug_bounds("copy-phone-1").is_some());
        // Nothing is editable yet: no Edit button.
        assert!(detail.versions.iter().all(|v| !v.editable));

        // Selecting another row shows that contact.
        click(cx, "contact-row-1");
        let shown = book.read_with(cx, |b, _| b.detail().map(|d| d.email.to_string()));
        assert_eq!(shown.as_deref(), Some("clara.novak@acme.example"));
    }

    #[gpui::test]
    fn new_mail_asks_for_a_draft_to_the_contact(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        click(cx, "space-contacts");
        let requests: Rc<RefCell<Vec<ComposeRequest>>> = Rc::default();
        let seen = requests.clone();
        let _subscription = cx.update(|_, cx| {
            cx.subscribe(&view, move |_, request: &ComposeRequest, _| {
                seen.borrow_mut().push(request.clone());
            })
        });
        click(cx, "contact-new-mail");
        assert_eq!(
            *requests.borrow(),
            [ComposeRequest::To(vec![Address::new(
                Some("Anna Example"),
                "anna@example.com"
            )])]
        );
    }

    #[gpui::test]
    fn show_mail_searches_the_mail_with_the_contact(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        click(cx, "space-contacts");
        click(cx, "contact-show-mail");
        assert_eq!(space(&view, cx), Space::Mail);
        let query = show_mail_query("anna@example.com");
        assert_eq!(
            query,
            r#"from:"anna@example.com" OR to:"anna@example.com" OR cc:"anna@example.com""#
        );
        let search = view.read_with(cx, |w, cx| w.search_text(cx));
        assert_eq!(search, query);
        let title = view.read_with(cx, |w, cx| {
            w.active_mail_view(cx)
                .map(|v| v.read(cx).thread_list().read(cx).title().to_string())
        });
        assert_eq!(title, Some(format!("Search: “{query}”")));
    }

    #[gpui::test]
    fn the_quick_filter_and_the_sort_button(cx: &mut TestAppContext) {
        let (view, cx) = open(cx);
        click(cx, "space-contacts");
        let book = book(&view, cx);
        cx.simulate_keystrokes("cmd-f");
        assert!(book.update_in(cx, |b, window, cx| b.filter_focused(window, cx)));
        cx.simulate_input("HOFER");
        cx.executor().advance_clock(FILTER_DEBOUNCE * 2);
        cx.run_until_parked();
        assert_eq!(emails(&book, cx), ["miriam@example.org"]);
        // By address too.
        let retype = |text: &str, cx: &mut VisualTestContext| {
            cx.simulate_keystrokes("secondary-a backspace");
            if !text.is_empty() {
                cx.simulate_input(text);
            }
            cx.executor().advance_clock(FILTER_DEBOUNCE * 2);
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
        };
        retype("club.example", cx);
        assert_eq!(
            emails(&book, cx),
            ["secretary@club.example", "kathrin@club.example"]
        );
        retype("nobody at all", cx);
        assert!(emails(&book, cx).is_empty());
        assert!(cx.debug_bounds("contacts-empty").is_some());
        retype("", cx);
        assert_eq!(emails(&book, cx).len(), 17);

        // Last used first: Anna was written to today.
        click(cx, "contact-sort");
        assert_eq!(book.read_with(cx, |b, _| b.sort()), ContactSort::LastUsed);
        assert_eq!(
            emails(&book, cx)[..2],
            ["anna@example.com", "jonas.berger@work.example"]
        );
        let saved = view.read_with(cx, |w, _| w.views_ui_state().address_book());
        assert_eq!(saved.sort, ContactSort::LastUsed);
        click(cx, "contact-sort");
        assert_eq!(book.read_with(cx, |b, _| b.sort()), ContactSort::Name);
    }

    #[gpui::test]
    fn without_contacts_the_space_says_where_they_come_from(cx: &mut TestAppContext) {
        let source = FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC));
        source.clear_contacts();
        let (view, cx) = open_with(source, cx);
        click(cx, "space-contacts");
        assert!(cx.debug_bounds("contacts-empty").is_some());
        let book = book(&view, cx);
        assert!(book.read_with(cx, |b, _| b.rows().is_empty() && b.detail().is_none()));
    }
}

//! A [`MailSource`] with synthetic, deterministic data.
//!
//! It backs demos (`MAZEMAIL_DEMO=1`), tests and the S1 measurements, which run
//! against: five accounts, unified folders, favorites, and one folder with 1,000,000
//! threads. Every row is computed from its thread id with a hash, so nothing is
//! stored per message: the 1M folder costs no memory beyond its key list, the same
//! way the real thread list only holds keys.
//!
//! Threads have one to eight messages with a generated reply structure (some branch),
//! written alternately by you and the other people of the thread, so the thread tree,
//! the unthreaded view and the Correspondents column ("sent" rows) have something to
//! show. Message ids encode their thread and position.
//!
//! Lists sort by any column like the store does: by what a row shows, text ignoring
//! case and `Re:`/`Fwd:` prefixes, empty text last, ties newest first.
//!
//! Changes (read, starred, archived, moved; per thread or per message; favorite
//! folders) are kept in memory. Archiving and moving hide a thread or message from its folder; the fake
//! doesn't add it to the Archives or target folder, but remembers where a move sent it
//! ([`FakeMailSource::moved_to`]).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use gpui::SharedString;
use mazemail_types::{
    AccountId, Address, Flags, FolderId, FolderRole, MessageId, Sensitive, ThreadId,
};
use time::{Duration, OffsetDateTime};

use crate::source::{
    AccountEntry, AccountKind, AttachmentInfo, Direction, FolderEntry, FolderKey, FolderTree,
    ListSort, MailSource, MessageBody, MessageDocument, MessageKeys, MessageRow, SenderClient,
    SortBy, SourceError, SourceStatus, SyncState, ThreadFilter, ThreadKeys, ThreadPredicate,
    ThreadRow, ThreadScope, ThreadTree, TreeNode, Verdict, Verification,
};

/// Number of threads in the large folder (Gmail (personal) / Archives).
pub const LARGE_FOLDER_THREADS: u32 = 1_000_000;

const DAY: i64 = 24 * 60 * 60;
const YEAR: i64 = 365 * DAY;

/// Static description of an account.
struct AccountSpec {
    name: &'static str,
    kind: AccountKind,
    address: &'static str,
}

const ACCOUNTS: &[AccountSpec] = &[
    AccountSpec {
        name: "Gmail (personal)",
        kind: AccountKind::Gmail,
        address: "philip@example.com",
    },
    AccountSpec {
        name: "Gmail (club)",
        kind: AccountKind::Gmail,
        address: "philip@club.example",
    },
    AccountSpec {
        name: "Microsoft 365",
        kind: AccountKind::Microsoft,
        address: "philip@work.example",
    },
    AccountSpec {
        name: "Personal IMAP",
        kind: AccountKind::Imap,
        address: "me@example.net",
    },
    AccountSpec {
        name: "Old ISP",
        kind: AccountKind::Imap,
        address: "philip@isp.example",
    },
];

/// Static description of a folder.
struct FolderSpec {
    account: usize,
    /// Index of the parent folder in [`FOLDERS`].
    parent: Option<usize>,
    name: &'static str,
    role: Option<FolderRole>,
    threads: u32,
    /// How far back the folder's mail reaches.
    span_secs: i64,
}

const fn folder(
    account: usize,
    parent: Option<usize>,
    name: &'static str,
    role: Option<FolderRole>,
    threads: u32,
    span_secs: i64,
) -> FolderSpec {
    FolderSpec {
        account,
        parent,
        name,
        role,
        threads,
        span_secs,
    }
}

use FolderRole::{Archive, Drafts, Inbox, Junk, Sent, Trash};

/// Every folder. Children follow their parent; the index is the folder's identity.
const FOLDERS: &[FolderSpec] = &[
    // 0..=7: Gmail (personal)
    folder(0, None, "Inbox", Some(Inbox), 4_200, 2 * YEAR),
    folder(0, None, "Drafts", Some(Drafts), 12, YEAR),
    folder(0, None, "Sent", Some(Sent), 8_400, 6 * YEAR),
    folder(
        0,
        None,
        "Archives",
        Some(Archive),
        LARGE_FOLDER_THREADS,
        12 * YEAR,
    ),
    folder(0, None, "Receipts", None, 1_800, 5 * YEAR),
    folder(0, None, "Newsletters", None, 12_000, 4 * YEAR),
    folder(0, None, "Junk", Some(Junk), 230, 30 * DAY),
    folder(0, None, "Trash", Some(Trash), 540, 60 * DAY),
    // 8..=13: Gmail (club)
    folder(1, None, "Inbox", Some(Inbox), 640, 2 * YEAR),
    folder(1, None, "Drafts", Some(Drafts), 2, YEAR),
    folder(1, None, "Sent", Some(Sent), 310, 4 * YEAR),
    folder(1, None, "Archives", Some(Archive), 5_200, 8 * YEAR),
    folder(1, None, "Junk", Some(Junk), 40, 30 * DAY),
    folder(1, None, "Trash", Some(Trash), 80, 60 * DAY),
    // 14..=23: Microsoft 365
    folder(2, None, "Inbox", Some(Inbox), 2_900, YEAR),
    folder(2, None, "Drafts", Some(Drafts), 6, YEAR),
    folder(2, None, "Sent", Some(Sent), 4_100, 3 * YEAR),
    folder(2, None, "Archives", Some(Archive), 48_000, 6 * YEAR),
    folder(2, Some(17), "2025", None, 9_000, YEAR),
    folder(2, None, "Customers", None, 3_400, 3 * YEAR),
    folder(2, Some(19), "Acme", None, 420, 2 * YEAR),
    folder(2, None, "To Do", None, 14, 30 * DAY),
    folder(2, None, "Junk", Some(Junk), 90, 30 * DAY),
    folder(2, None, "Trash", Some(Trash), 260, 60 * DAY),
    // 24..=27: Personal IMAP
    folder(3, None, "Inbox", Some(Inbox), 380, 3 * YEAR),
    folder(3, None, "Sent", Some(Sent), 150, 3 * YEAR),
    folder(3, None, "Junk", Some(Junk), 15, 30 * DAY),
    folder(3, None, "Trash", Some(Trash), 20, 60 * DAY),
    // 28..=29: Old ISP
    folder(4, None, "Old mail", None, 25_000, 15 * YEAR),
    folder(4, None, "Trash", Some(Trash), 0, DAY),
];

/// Favorites at the start, by folder index, in the order they were added (To Do,
/// Acme, Receipts).
const FAVORITES: &[usize] = &[21, 20, 4];

/// Roles shown under Unified Folders, in Thunderbird's order.
const UNIFIED_ROLES: &[FolderRole] = &[Inbox, Drafts, Sent, Archive, Junk, Trash];

/// People and services that send the synthetic mail.
const SENDERS: &[(&str, &str)] = &[
    ("Anna Example", "anna@example.com"),
    ("AWS Billing", "billing@aws.example"),
    ("GitHub", "notifications@github.example"),
    ("Jonas Berger", "jonas.berger@work.example"),
    ("Club Secretary", "secretary@club.example"),
    ("Miriam Hofer", "miriam@example.org"),
    ("Lukas Steiner", "lukas@steiner.example"),
    ("Sophie Wagner", "sophie.wagner@work.example"),
    ("Deutsche Bahn", "noreply@bahn.example"),
    ("Hetzner Online", "billing@hetzner.example"),
    ("Linear", "notifications@linear.example"),
    ("Elena Rossi", "elena.rossi@acme.example"),
    ("Tomás García", "tomas@garcia.example"),
    ("Newsletter Rust Weekly", "hello@rustweekly.example"),
    ("Stripe", "receipts@stripe.example"),
    ("Paul Huber", "paul.huber@example.net"),
    ("Kathrin Maier", "kathrin@club.example"),
    ("Apple", "no_reply@apple.example"),
    ("Google", "no-reply@accounts.google.example"),
    ("Zed Industries", "hi@zed.example"),
    ("Hannah Gruber", "hannah.gruber@work.example"),
    ("Felix Bauer", "felix@bauer.example"),
    ("Amazon", "shipment-tracking@amazon.example"),
    ("Austrian Airlines", "booking@austrian.example"),
    ("Clara Novak", "clara.novak@acme.example"),
    ("distr CI", "ci@distr.example"),
    ("Markus Pichler", "markus@pichler.example"),
    ("Julia Lechner", "julia.lechner@example.com"),
    ("Booking.com", "customer.service@booking.example"),
    ("Nina Fischer", "nina@fischer.example"),
];

const SUBJECTS: &[&str] = &[
    "Q3 offsite",
    "Invoice #{n}",
    "Weekly digest: week {w}",
    "Training Thursday",
    "Your order #{n} has shipped",
    "[distr] Pull request #{n}: {topic}",
    "Meeting notes {topic}",
    "Quarterly numbers",
    "Lunch on {day}?",
    "Your receipt from {service}",
    "Security alert: new sign-in on {device}",
    "This week in {topic}",
    "Contract renewal {year}",
    "Flight confirmation {code}",
    "Photos from the weekend",
    "Club fees {year}",
    "Build failed on main (#{n})",
    "Reminder: appointment on {day}",
    "Agenda for {day}",
    "Your monthly statement",
    "Welcome to {service}",
    "Updated plan for the offsite",
    "Can you review the {topic} draft?",
    "Re-scheduling our call",
    "Ticket #{n}: {topic}",
    "Your booking in {city}",
    "Feedback on the {topic} proposal",
    "Season kickoff in {city}",
    "Server maintenance on {day}",
    "Question about invoice #{n}",
    "Slides for {day}",
    "Happy birthday!",
    "{service} price changes",
    "Board meeting minutes",
    "Your trip to {city}",
    "New comment on {topic}",
];

const TOPICS: &[&str] = &[
    "release process",
    "auto-update",
    "onboarding flow",
    "Kubernetes upgrade",
    "pricing page",
    "roadmap",
    "search performance",
    "IMAP sync",
    "hiring plan",
    "budget 2027",
    "customer portal",
    "data retention",
];
const DAYS: &[&str] = &["Monday", "Tuesday", "Wednesday", "Thursday", "Friday"];
const SERVICES: &[&str] = &[
    "Hetzner", "Stripe", "Apple", "Spotify", "GitHub", "Fastmail", "Linear", "Figma",
];
const CITIES: &[&str] = &[
    "Vienna",
    "Graz",
    "Linz",
    "Salzburg",
    "Munich",
    "Berlin",
    "Lisbon",
    "Copenhagen",
];
const DEVICES: &[&str] = &["Mac", "iPhone", "Windows", "Linux", "iPad"];

const SENTENCES: &[&str] = &[
    "attached the updated plan for the offsite.",
    "can you check the dates before Friday?",
    "I went through the numbers again and they look much better than last quarter.",
    "the build on main is green again after the dependency bump.",
    "we moved the meeting to next week so everyone can join.",
    "please find the invoice for last month attached.",
    "let me know if anything is unclear or if you need more details.",
    "the venue confirmed the booking for 24 people.",
    "I added a few comments in the document, mostly small things.",
    "your order is on its way and should arrive in two to three days.",
    "thanks again for the quick reply yesterday.",
    "we're planning to roll this out to all customers in November.",
    "training starts at 18:30, please be on time.",
    "the new pricing applies from the next billing cycle.",
    "I think we can skip the second round of reviews this time.",
    "here are the notes from today's call.",
];

/// SplitMix64: a fast, well-distributed hash for synthetic data.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// Mail clients of fake senders: (name, header); `None` for clients that leave no trace.
const CLIENTS: &[Option<(&str, &str)>] = &[
    Some((
        "Thunderbird 128.3 (Linux)",
        "User-Agent: Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Thunderbird/128.3.1",
    )),
    Some(("Apple Mail (iPhone)", "X-Mailer: iPhone Mail (22B83)")),
    Some((
        "Gmail",
        "Message-ID: <CAF=+kL3x9ZQ1+Yb7mQyH0a@mail.gmail.com>",
    )),
    Some(("Outlook 16.0", "X-Mailer: Microsoft Outlook 16.0")),
    None,
];

fn pick<T: Copy>(items: &[T], bits: u64) -> T {
    items[(bits % items.len() as u64) as usize]
}

/// Encodes folder index and row into a thread id.
fn thread_id(folder: usize, row: u32) -> ThreadId {
    ThreadId(((folder as i64 + 1) << 32) | i64::from(row))
}

/// Decodes a thread id into folder index and row.
fn decode(id: ThreadId) -> Option<(usize, u32)> {
    let folder = usize::try_from((id.0 >> 32) - 1).ok()?;
    let row = u32::try_from(id.0 & 0xffff_ffff).ok()?;
    (folder < FOLDERS.len() && row < FOLDERS[folder].threads).then_some((folder, row))
}

/// Bits of a message id that hold the message's index in its thread.
const MESSAGE_BITS: u32 = 4;

/// The id of message `index` (oldest first) of a thread.
fn message_id(thread: ThreadId, index: u32) -> MessageId {
    MessageId((thread.0 << MESSAGE_BITS) | i64::from(index))
}

/// Decodes a message id into its thread and index.
fn decode_message(id: MessageId) -> Option<(ThreadId, u32)> {
    let thread = ThreadId(id.0 >> MESSAGE_BITS);
    let index = u32::try_from(id.0 & ((1 << MESSAGE_BITS) - 1)).ok()?;
    decode(thread)?;
    (index < FakeMailSource::message_count(thread)).then_some((thread, index))
}

fn folder_id(folder: usize) -> FolderId {
    FolderId(folder as i64 + 1)
}

fn folder_index(id: FolderId) -> Option<usize> {
    usize::try_from(id.0 - 1)
        .ok()
        .filter(|ix| *ix < FOLDERS.len())
}

/// Mutable state: changes made through the UI.
#[derive(Clone, Default)]
struct State {
    /// Thread flags that differ from the generated ones (a thread command sets every
    /// message of the thread).
    flags: HashMap<ThreadId, Flags>,
    /// Message flags changed one message at a time, per thread and message index.
    message_flags: HashMap<ThreadId, HashMap<u32, Flags>>,
    archived: HashSet<ThreadId>,
    archived_messages: HashSet<MessageId>,
    /// Where moved threads and messages went.
    moves: HashMap<MoveItem, FolderId>,
    /// Unread count changes per folder, relative to the generated counts.
    unread_delta: Vec<i64>,
    /// Favorite folders, by folder index.
    favorites: HashSet<usize>,
}

/// A thread or message the fake moved, see [`FakeMailSource::moved_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoveItem {
    /// A whole thread.
    Thread(ThreadId),
    /// One message.
    Message(MessageId),
}

/// Synthetic mail for development and benchmarks. See the module docs.
pub struct FakeMailSource {
    now: OffsetDateTime,
    unread_base: Vec<u64>,
    state: RwLock<State>,
    /// The state before the last change, for [`MailSource::undo`].
    undo: Mutex<Option<State>>,
    /// Unfiltered key lists, built on first use.
    key_cache: Mutex<HashMap<(ThreadScope, ListSort), ThreadKeys>>,
    message_key_cache: Mutex<HashMap<(ThreadScope, ListSort), MessageKeys>>,
}

impl std::fmt::Debug for FakeMailSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeMailSource")
            .field("now", &self.now)
            .field("folders", &FOLDERS.len())
            .finish_non_exhaustive()
    }
}

impl Default for FakeMailSource {
    fn default() -> Self {
        Self::new()
    }
}

/// Who wrote a message: you, or one of the two other people of the thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Author {
    Me,
    Them(usize),
}

impl FakeMailSource {
    /// Synthetic mail with the newest message a few minutes old.
    pub fn new() -> Self {
        Self::with_now(OffsetDateTime::now_utc())
    }

    /// Synthetic mail relative to a fixed time, for reproducible tests.
    pub fn with_now(now: OffsetDateTime) -> Self {
        let unread_base = (0..FOLDERS.len())
            .map(|folder| {
                (0..FOLDERS[folder].threads)
                    .filter(|row| !Self::generated_flags(folder, *row).contains(Flags::SEEN))
                    .count() as u64
            })
            .collect();
        Self {
            now,
            unread_base,
            state: RwLock::new(State {
                unread_delta: vec![0; FOLDERS.len()],
                favorites: FAVORITES.iter().copied().collect(),
                ..State::default()
            }),
            undo: Mutex::new(None),
            key_cache: Mutex::new(HashMap::new()),
            message_key_cache: Mutex::new(HashMap::new()),
        }
    }

    /// The key of the 1,000,000-thread folder.
    pub fn large_folder() -> FolderKey {
        FolderKey::Folder(folder_id(3))
    }

    /// The folder a thread or message was moved to, if it was.
    pub fn moved_to(&self, item: MoveItem) -> Option<FolderId> {
        self.read_state().moves.get(&item).copied()
    }

    /// The account of a folder index, as the folder tree numbers them.
    fn account_id(folder: usize) -> AccountId {
        AccountId(FOLDERS[folder].account as i64 + 1)
    }

    /// Checks that `to` is a folder of `account` (moves stay within an account).
    fn check_target(account: AccountId, to: FolderId) -> Result<(), SourceError> {
        match folder_index(to) {
            Some(ix) if Self::account_id(ix) == account => Ok(()),
            _ => Err(SourceError::FolderNotFound),
        }
    }

    fn hash(folder: usize, row: u32) -> u64 {
        mix(((folder as u64) << 32) | u64::from(row))
    }

    /// Messages in a generated thread: 30 % of threads have 2 to 8.
    fn message_count(thread: ThreadId) -> u32 {
        decode(thread).map_or(0, |(folder, row)| {
            let h = Self::hash(folder, row);
            if (h >> 44) % 100 < 30 {
                2 + ((h >> 50) % 7) as u32
            } else {
                1
            }
        })
    }

    fn generated_flags(folder: usize, row: u32) -> Flags {
        let spec = &FOLDERS[folder];
        let h = Self::hash(folder, row);
        let mut flags = Flags::empty();
        let unread = match spec.role {
            Some(Sent | Drafts) => false,
            Some(Junk) => (h >> 8) % 100 < 60,
            _ if row < 30 => (h >> 8) % 100 < 45,
            _ => (h >> 8) % 1000 < 3,
        };
        if !unread {
            flags |= Flags::SEEN;
        }
        if (h >> 20) % 100 < 3 {
            flags |= Flags::FLAGGED;
        }
        if (h >> 28) % 100 < 12 {
            flags |= Flags::ANSWERED;
        }
        if spec.role == Some(Drafts) {
            flags |= Flags::DRAFT;
        }
        flags
    }

    /// The flags of message `index`: set one by one, else from the thread's flags (only
    /// the newest message is unread or starred).
    fn message_flags(state: &State, thread: ThreadId, index: u32) -> Flags {
        if let Some(flags) = state.message_flags.get(&thread).and_then(|m| m.get(&index)) {
            return *flags;
        }
        let Some((folder, row)) = decode(thread) else {
            return Flags::SEEN;
        };
        let base = state
            .flags
            .get(&thread)
            .copied()
            .unwrap_or_else(|| Self::generated_flags(folder, row));
        if index + 1 == Self::message_count(thread) {
            base
        } else {
            (base | Flags::SEEN) - Flags::FLAGGED
        }
    }

    /// Thread flags: unread if a message is, starred if a message is.
    fn flags(&self, state: &State, id: ThreadId, folder: usize, row: u32) -> Flags {
        let base = state
            .flags
            .get(&id)
            .copied()
            .unwrap_or_else(|| Self::generated_flags(folder, row));
        if !state.message_flags.contains_key(&id) && state.archived_messages.is_empty() {
            return base;
        }
        let mut flags = base - Flags::FLAGGED - Flags::SEEN;
        let mut all_seen = true;
        for index in self.live_messages(state, id) {
            let m = Self::message_flags(state, id, index);
            all_seen &= m.contains(Flags::SEEN);
            flags |= m & Flags::FLAGGED;
        }
        if all_seen {
            flags |= Flags::SEEN;
        }
        flags
    }

    /// The indexes of the thread's messages that aren't archived, oldest first.
    fn live_messages(&self, state: &State, thread: ThreadId) -> impl Iterator<Item = u32> {
        let archived: Vec<u32> = if state.archived_messages.is_empty() {
            Vec::new()
        } else {
            (0..Self::message_count(thread))
                .filter(|i| state.archived_messages.contains(&message_id(thread, *i)))
                .collect()
        };
        (0..Self::message_count(thread)).filter(move |i| !archived.contains(i))
    }

    fn date(&self, folder: usize, row: u32) -> OffsetDateTime {
        let spec = &FOLDERS[folder];
        let cadence = (spec.span_secs / i64::from(spec.threads.max(1))).max(30);
        // Less than half a cadence, so a thread's messages fit before the next thread.
        let jitter = (Self::hash(folder, row) >> 3) as i64 % (cadence / 2);
        // Strictly decreasing with the row, so key order is date order.
        self.now - Duration::seconds(i64::from(row) * cadence + jitter + 240)
    }

    /// Date of message `index` of a thread: the newest is the thread's date, older
    /// ones are spread over less than half a cadence before it.
    fn message_date(&self, thread: ThreadId, index: u32) -> OffsetDateTime {
        let Some((folder, row)) = decode(thread) else {
            return OffsetDateTime::UNIX_EPOCH;
        };
        let spec = &FOLDERS[folder];
        let cadence = (spec.span_secs / i64::from(spec.threads.max(1))).max(30);
        let count = Self::message_count(thread);
        let step = (cadence / (2 * i64::from(count.max(1)))).max(1);
        self.date(folder, row) - Duration::seconds(i64::from(count - 1 - index) * step)
    }

    fn fill(template: &str, h: u64, date: OffsetDateTime) -> String {
        let mut out = template.to_owned();
        let replacements: [(&str, String); 9] = [
            ("{n}", (1000 + (h >> 7) % 9000).to_string()),
            ("{w}", (1 + (h >> 11) % 52).to_string()),
            ("{topic}", pick(TOPICS, h >> 13).to_owned()),
            ("{day}", pick(DAYS, h >> 17).to_owned()),
            ("{service}", pick(SERVICES, h >> 19).to_owned()),
            ("{year}", date.year().to_string()),
            ("{code}", format!("OS{}", 100 + (h >> 23) % 900)),
            ("{city}", pick(CITIES, h >> 29).to_owned()),
            ("{device}", pick(DEVICES, h >> 31).to_owned()),
        ];
        for (placeholder, value) in replacements {
            if out.contains(placeholder) {
                out = out.replace(placeholder, &value);
            }
        }
        out
    }

    /// The thread's subject without a reply prefix.
    fn base_subject(&self, folder: usize, row: u32) -> String {
        let h = Self::hash(folder, row);
        Self::fill(pick(SUBJECTS, h >> 37), h, self.date(folder, row))
    }

    /// The two other people of a thread.
    fn people(folder: usize, row: u32) -> [(&'static str, &'static str); 2] {
        let h = Self::hash(folder, row);
        let first = pick(SENDERS, h);
        let mut second = pick(SENDERS, h >> 9);
        if second == first {
            second = pick(SENDERS, (h >> 9) + 1);
        }
        [first, second]
    }

    /// Who wrote message `index`: in Sent and Drafts the newest message is yours, else
    /// the first person's; replies alternate between you and the others.
    fn author(folder: usize, row: u32, index: u32, count: u32) -> Author {
        let from_newest = count - 1 - index;
        let outgoing_folder = matches!(FOLDERS[folder].role, Some(Sent | Drafts));
        let mine = from_newest.is_multiple_of(2) == outgoing_folder;
        if mine {
            Author::Me
        } else if index == 0 || from_newest == 0 {
            Author::Them(0)
        } else {
            Author::Them((Self::hash(folder, row) >> (index * 3)) as usize % 2)
        }
    }

    fn me(folder: usize) -> Address {
        Address::new(Some("Philip"), ACCOUNTS[FOLDERS[folder].account].address)
    }

    /// Sender and recipients of message `index`.
    fn addresses(folder: usize, row: u32, index: u32, count: u32) -> (Address, Vec<Address>) {
        let people = Self::people(folder, row);
        let them = |i: usize| Address::new(Some(people[i].0), people[i].1);
        match Self::author(folder, row, index, count) {
            Author::Me => {
                let mut to = vec![them(0)];
                if count > 2 {
                    to.push(them(1));
                }
                (Self::me(folder), to)
            }
            Author::Them(i) => (them(i), vec![Self::me(folder)]),
        }
    }

    fn account_name(folder: usize) -> SharedString {
        ACCOUNTS[FOLDERS[folder].account].name.into()
    }

    fn tags(h: u64) -> Vec<SharedString> {
        match (h >> 56) % 40 {
            0 => vec!["Important".into()],
            1 => vec!["To Do".into()],
            2 => vec!["Work".into(), "Later".into()],
            _ => Vec::new(),
        }
    }

    fn row(&self, state: &State, id: ThreadId) -> Option<ThreadRow> {
        let (folder, row) = decode(id)?;
        if state.archived.contains(&id) {
            return None;
        }
        let h = Self::hash(folder, row);
        let count = Self::message_count(id);
        let live: Vec<u32> = self.live_messages(state, id).collect();
        let newest = *live.last()?;
        let mut subject = self.base_subject(folder, row);
        if count > 1 {
            subject.insert_str(0, "Re: ");
        }
        let (from, to) = Self::addresses(folder, row, newest, count);
        let has_attachments = (h >> 36) % 100 < 14;
        let size = 2_000
            + (h >> 13) % 60_000
            + if has_attachments {
                (h >> 17) % 4_000_000
            } else {
                0
            };
        Some(ThreadRow {
            id,
            subject: Sensitive::new(subject.into()),
            sender: Sensitive::new(display(&from).into()),
            recipients: Sensitive::new(join(&to).into()),
            outgoing: Self::author(folder, row, newest, count) == Author::Me,
            date: self.message_date(id, newest),
            flags: self.flags(state, id, folder, row),
            has_attachments,
            message_count: live.len() as u32,
            size,
            account: Self::account_name(folder),
            tags: Self::tags(h),
        })
    }

    fn message_row(&self, state: &State, id: MessageId) -> Option<MessageRow> {
        let (thread, index) = decode_message(id)?;
        let (folder, row) = decode(thread)?;
        if state.archived.contains(&thread) || state.archived_messages.contains(&id) {
            return None;
        }
        let count = Self::message_count(thread);
        let h = mix(Self::hash(folder, row) ^ u64::from(index + 1));
        let mut subject = self.base_subject(folder, row);
        if index > 0 {
            subject.insert_str(0, "Re: ");
        }
        let (from, to) = Self::addresses(folder, row, index, count);
        let thread_attachments = (Self::hash(folder, row) >> 36) % 100 < 14;
        let has_attachments = thread_attachments && (index + 1 == count || h.is_multiple_of(3));
        Some(MessageRow {
            id,
            thread,
            subject: Sensitive::new(subject.into()),
            sender: Sensitive::new(display(&from).into()),
            recipients: Sensitive::new(join(&to).into()),
            outgoing: Self::author(folder, row, index, count) == Author::Me,
            date: self.message_date(thread, index),
            flags: Self::message_flags(state, thread, index),
            has_attachments,
            size: 1_500
                + (h >> 13) % 40_000
                + if has_attachments {
                    (h >> 17) % 2_000_000
                } else {
                    0
                },
            account: Self::account_name(folder),
            tags: if index + 1 == count {
                Self::tags(Self::hash(folder, row))
            } else {
                Vec::new()
            },
        })
    }

    /// The generated reply structure: message `i` replies to the one before it or, now
    /// and then, to an earlier one, so trees branch.
    fn parent_index(thread: ThreadId, index: u32) -> Option<u32> {
        if index == 0 {
            return None;
        }
        let h = mix(thread.0 as u64 ^ u64::from(index) << 40);
        Some(if h.is_multiple_of(3) {
            (h >> 8) as u32 % index
        } else {
            index - 1
        })
    }

    fn tree(&self, state: &State, thread: ThreadId) -> Vec<TreeNode> {
        let live: Vec<u32> = self.live_messages(state, thread).collect();
        // Parents that were archived are replaced by their nearest live ancestor.
        let parent_of = |mut index: u32| loop {
            match Self::parent_index(thread, index) {
                None => break None,
                Some(p) if live.contains(&p) => break Some(p),
                Some(p) => index = p,
            }
        };
        let mut out = Vec::with_capacity(live.len());
        let mut stack: Vec<(u32, u16)> = live
            .iter()
            .rev()
            .filter(|i| parent_of(**i).is_none())
            .map(|i| (*i, 0))
            .collect();
        while let Some((index, depth)) = stack.pop() {
            out.push(TreeNode {
                message: message_id(thread, index),
                parent: parent_of(index).map(|p| message_id(thread, p)),
                depth,
            });
            for child in live.iter().rev().filter(|c| parent_of(**c) == Some(index)) {
                stack.push((*child, depth + 1));
            }
        }
        out
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Remembers the state before a change for [`MailSource::undo`].
    fn checkpoint(&self, state: &State) {
        *self.undo.lock().unwrap_or_else(PoisonError::into_inner) = Some(state.clone());
    }

    fn invalidate_keys(&self) {
        self.key_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        self.message_key_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn members(role: FolderRole) -> impl Iterator<Item = usize> {
        (0..FOLDERS.len()).filter(move |ix| FOLDERS[*ix].role == Some(role))
    }

    /// Folder indexes behind a scope.
    fn scope_folders(scope: &ThreadScope) -> Result<Vec<usize>, SourceError> {
        match scope {
            ThreadScope::Folder(FolderKey::Folder(id)) => folder_index(*id)
                .map(|ix| vec![ix])
                .ok_or(SourceError::FolderNotFound),
            ThreadScope::Folder(FolderKey::Unified(role)) => Ok(Self::members(*role).collect()),
            ThreadScope::AllFolders => Ok((0..FOLDERS.len()).collect()),
        }
    }

    /// Keys of one folder, newest first, after the filter.
    fn folder_keys(&self, state: &State, folder: usize, filter: &ThreadFilter) -> Vec<ThreadId> {
        let rows = 0..FOLDERS[folder].threads;
        let needs_text = filter.text.as_deref().is_some_and(|t| !t.trim().is_empty());
        if filter.is_empty() {
            rows.map(|row| thread_id(folder, row))
                .filter(|id| state.archived.is_empty() || !state.archived.contains(id))
                .collect()
        } else if needs_text {
            rows.filter_map(|row| self.row(state, thread_id(folder, row)))
                .filter(|row| filter.matches(row))
                .map(|row| row.id)
                .collect()
        } else {
            // Flag-only filters don't need the strings.
            rows.filter(|row| {
                let id = thread_id(folder, *row);
                if state.archived.contains(&id) {
                    return false;
                }
                let flags = self.flags(state, id, folder, *row);
                let attachments = (Self::hash(folder, *row) >> 36) % 100 < 14;
                (!filter.unread || !flags.contains(Flags::SEEN))
                    && (!filter.starred || flags.contains(Flags::FLAGGED))
                    && (!filter.attachments || attachments)
            })
            .map(|row| thread_id(folder, row))
            .collect()
        }
    }

    /// Message keys of one folder, newest first, after the filter.
    fn folder_message_keys(
        &self,
        state: &State,
        folder: usize,
        filter: &ThreadFilter,
    ) -> Vec<MessageId> {
        let mut out = Vec::with_capacity(FOLDERS[folder].threads as usize * 2);
        let needs_text = filter.text.as_deref().is_some_and(|t| !t.trim().is_empty());
        for row in 0..FOLDERS[folder].threads {
            let thread = thread_id(folder, row);
            if state.archived.contains(&thread) {
                continue;
            }
            for index in (0..Self::message_count(thread)).rev() {
                let id = message_id(thread, index);
                if !state.archived_messages.is_empty() && state.archived_messages.contains(&id) {
                    continue;
                }
                if !filter.is_empty() {
                    let keep = if needs_text {
                        self.message_row(state, id)
                            .is_some_and(|row| filter.matches_message(&row))
                    } else {
                        let flags = Self::message_flags(state, thread, index);
                        let attachments = filter.attachments
                            && self
                                .message_row(state, id)
                                .is_some_and(|row| row.has_attachments);
                        (!filter.unread || !flags.contains(Flags::SEEN))
                            && (!filter.starred || flags.contains(Flags::FLAGGED))
                            && (!filter.attachments || attachments)
                    };
                    if !keep {
                        continue;
                    }
                }
                out.push(id);
            }
        }
        out
    }

    /// Merges per-folder key lists (each newest first) into one, newest first.
    fn merge_by_date<K: Copy>(lists: Vec<Vec<K>>, date_of: impl Fn(K) -> OffsetDateTime) -> Vec<K> {
        let mut lists: Vec<_> = lists.into_iter().filter(|l| !l.is_empty()).collect();
        if lists.len() <= 1 {
            return lists.pop().unwrap_or_default();
        }
        let total = lists.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total);
        let mut cursors = vec![0usize; lists.len()];
        let mut heads: Vec<Option<OffsetDateTime>> = lists
            .iter()
            .map(|l| l.first().map(|id| date_of(*id)))
            .collect();
        loop {
            let next = heads
                .iter()
                .enumerate()
                .filter_map(|(ix, date)| date.map(|d| (ix, d)))
                .max_by_key(|(_, date)| *date);
            let Some((ix, _)) = next else { break };
            out.push(lists[ix][cursors[ix]]);
            cursors[ix] += 1;
            heads[ix] = lists[ix].get(cursors[ix]).map(|id| date_of(*id));
        }
        out
    }

    fn thread_date(&self, id: ThreadId) -> OffsetDateTime {
        decode(id).map_or(OffsetDateTime::UNIX_EPOCH, |(f, r)| self.date(f, r))
    }

    fn folder_entry(&self, folder: usize) -> FolderEntry {
        let spec = &FOLDERS[folder];
        FolderEntry {
            key: FolderKey::Folder(folder_id(folder)),
            name: spec.name.into(),
            role: spec.role,
            children: (0..FOLDERS.len())
                .filter(|ix| FOLDERS[*ix].parent == Some(folder))
                .map(|ix| self.folder_entry(ix))
                .collect(),
        }
    }

    fn body(&self, subject: &str, h: u64, from: &Address, to: &Address, quote: bool) -> String {
        let first_name = |a: &Address| {
            let name = a.name.as_deref().unwrap_or(&a.email);
            name.split_whitespace().next().unwrap_or(name).to_owned()
        };
        let greeting = first_name(to);
        let sentence = |bits: u64| {
            let s = pick(SENTENCES, bits);
            let mut chars = s.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        };
        let mut html = format!("<p>Hi {greeting},</p>");
        html.push_str(&format!("<p>{} {}</p>", sentence(h >> 5), sentence(h >> 9)));
        if (h >> 40).is_multiple_of(3) {
            html.push_str("<ul>");
            for i in 0..3 {
                html.push_str(&format!("<li>{}</li>", sentence(h >> (12 + i * 4))));
            }
            html.push_str("</ul>");
        }
        html.push_str(&format!("<p>{}</p>", sentence(h >> 21)));
        if subject.contains("Invoice") {
            html.push_str(
                "<table><tr><th>Item</th><th>Amount</th></tr>\
                 <tr><td>Cloud servers</td><td>€ 182.40</td></tr>\
                 <tr><td>Backups</td><td>€ 12.00</td></tr>\
                 <tr><td><b>Total</b></td><td><b>€ 194.40</b></td></tr></table>",
            );
        }
        html.push_str(&format!("<p>Best,<br>{}</p>", first_name(from)));
        if quote {
            html.push_str(&format!(
                "<blockquote><p>{greeting} wrote:</p><p>{}</p></blockquote>",
                sentence(h >> 27)
            ));
        }
        html
    }

    fn document(&self, state: &State, id: MessageId) -> Result<MessageDocument, SourceError> {
        let row = self
            .message_row(state, id)
            .ok_or(SourceError::MessageNotFound(id))?;
        let (thread, index) = decode_message(id).ok_or(SourceError::MessageNotFound(id))?;
        let (folder, r) = decode(thread).ok_or(SourceError::MessageNotFound(id))?;
        let count = Self::message_count(thread);
        let h = mix(Self::hash(folder, r) ^ u64::from(index + 1));
        let (from, to) = Self::addresses(folder, r, index, count);
        let cc = if (h >> 45).is_multiple_of(4) {
            let (cc_name, cc_email) = pick(SENDERS, h >> 3);
            vec![Address::new(Some(cc_name), cc_email)]
        } else {
            Vec::new()
        };
        let attachments = if row.has_attachments {
            let names = [
                "plan-v3.pdf",
                "invoice.pdf",
                "photos.zip",
                "notes.docx",
                "slides.key",
            ];
            vec![AttachmentInfo {
                name: Sensitive::new(pick(&names, h >> 33).into()),
                size: row.size.saturating_sub(1_500),
            }]
        } else {
            Vec::new()
        };
        let body = self.body(row.subject.expose(), h, &from, &to[0], index > 0);
        Ok(MessageDocument {
            thread,
            message: id,
            subject: row.subject.clone(),
            from: Sensitive::new(from),
            to: Sensitive::new(to),
            cc: Sensitive::new(cc),
            date: row.date,
            body: MessageBody::Html(Sensitive::new(SharedString::from(body))),
            attachments,
            verification: Verification::Checked {
                verdict: Verdict::Verified,
                dkim: "pass".into(),
                spf: "pass".into(),
                dmarc: "pass".into(),
                explanation: "The sender's domain signed this message.".into(),
            },
            client: Sensitive::new(pick(CLIENTS, h >> 51).map(|(name, header)| SenderClient {
                name: name.into(),
                header: header.into(),
            })),
        })
    }

    /// Records an unread change of a thread in its folder's count.
    fn count_unread_change(&self, state: &mut State, thread: ThreadId, was_unread: bool) {
        let Some((folder, row)) = decode(thread) else {
            return;
        };
        let is_unread = !self.flags(state, thread, folder, row).contains(Flags::SEEN);
        if was_unread != is_unread && !state.archived.contains(&thread) {
            state.unread_delta[folder] += if is_unread { 1 } else { -1 };
        }
    }
}

/// What a row sorts by.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    Number(u64),
    /// Lowercased; `None` (empty) sorts last both ways.
    Text(Option<String>),
}

struct SortRow {
    id: i64,
    date: OffsetDateTime,
    value: SortValue,
}

/// The columns of a thread or message row.
struct Fields<'a> {
    flags: Flags,
    attachment: bool,
    size: u64,
    date: OffsetDateTime,
    subject: &'a str,
    sender: &'a str,
    recipients: &'a str,
    correspondents: &'a str,
    tags: &'a [SharedString],
    account: &'a str,
}

impl Fields<'_> {
    /// The value of the column `by` sorts by, as the store computes it.
    fn sort_value(&self, by: SortBy) -> SortValue {
        let text = |text: &str| {
            let text = text.trim().to_lowercase();
            SortValue::Text((!text.is_empty()).then_some(text))
        };
        match by {
            SortBy::Date | SortBy::Received => {
                SortValue::Number(self.date.unix_timestamp().max(0) as u64)
            }
            SortBy::Star => SortValue::Number(u64::from(self.flags.contains(Flags::FLAGGED))),
            SortBy::Unread => SortValue::Number(u64::from(!self.flags.contains(Flags::SEEN))),
            SortBy::Attachment => SortValue::Number(u64::from(self.attachment)),
            SortBy::Size => SortValue::Number(self.size),
            SortBy::Subject => {
                let mut rest = self.subject.trim();
                while let Some((prefix, after)) = rest.split_once(':') {
                    let prefix = prefix.trim().to_lowercase();
                    if !["re", "fwd", "fw", "aw", "wg"].contains(&prefix.as_str()) {
                        break;
                    }
                    rest = after.trim_start();
                }
                text(rest)
            }
            SortBy::Correspondents => text(self.correspondents),
            SortBy::Sender => text(self.sender),
            SortBy::Recipients => text(self.recipients),
            SortBy::Tags => text(&self.tags.join(", ")),
            SortBy::Account => text(self.account),
        }
    }
}

/// The indexes of `rows` in `sort`'s order: by value (empty text last), then newest
/// first, then by id.
fn sorted<R>(rows: &[R], sort: ListSort, key: impl Fn(&R) -> SortRow) -> Vec<usize> {
    let keys: Vec<SortRow> = rows.iter().map(key).collect();
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|a, b| {
        let (a, b) = (&keys[*a], &keys[*b]);
        let by_value = match (&a.value, &b.value) {
            (SortValue::Text(None), SortValue::Text(Some(_))) => std::cmp::Ordering::Greater,
            (SortValue::Text(Some(_)), SortValue::Text(None)) => std::cmp::Ordering::Less,
            (x, y) if sort.descending => y.cmp(x),
            (x, y) => x.cmp(y),
        };
        by_value
            .then_with(|| b.date.cmp(&a.date))
            .then_with(|| b.id.cmp(&a.id))
    });
    order
}

/// How the list shows a person.
fn display(address: &Address) -> String {
    address
        .name
        .clone()
        .unwrap_or_else(|| address.email.clone())
}

fn join(addresses: &[Address]) -> String {
    addresses.iter().map(display).collect::<Vec<_>>().join(", ")
}

impl MailSource for FakeMailSource {
    fn folder_tree(&self) -> FolderTree {
        let unified = UNIFIED_ROLES
            .iter()
            .map(|role| FolderEntry {
                key: FolderKey::Unified(*role),
                name: match role {
                    Inbox => "Inbox",
                    Drafts => "Drafts",
                    Sent => "Sent",
                    Archive => "Archives",
                    Junk => "Junk",
                    _ => "Trash",
                }
                .into(),
                role: Some(*role),
                children: Vec::new(),
            })
            .collect();
        // Like the core: account order, then tree order (children follow their parent
        // in `FOLDERS`).
        let favorite = self.read_state().favorites.clone();
        let favorites = (0..FOLDERS.len())
            .filter(|ix| favorite.contains(ix))
            .map(|ix| {
                let mut entry = self.folder_entry(ix);
                entry.children.clear();
                entry
            })
            .collect();
        let accounts = ACCOUNTS
            .iter()
            .enumerate()
            .map(|(account, spec)| AccountEntry {
                id: AccountId(account as i64 + 1),
                name: spec.name.into(),
                kind: spec.kind,
                folders: (0..FOLDERS.len())
                    .filter(|ix| FOLDERS[*ix].account == account && FOLDERS[*ix].parent.is_none())
                    .map(|ix| self.folder_entry(ix))
                    .collect(),
            })
            .collect();
        FolderTree {
            favorites,
            unified,
            accounts,
        }
    }

    fn set_favorite(&self, folder: FolderId, favorite: bool) -> Result<(), SourceError> {
        let ix = folder_index(folder).ok_or(SourceError::FolderNotFound)?;
        let mut state = self.write_state();
        if favorite {
            state.favorites.insert(ix);
        } else {
            state.favorites.remove(&ix);
        }
        Ok(())
    }

    fn unread_count(&self, folder: &FolderKey) -> u64 {
        let state = self.read_state();
        let count =
            |ix: usize| (self.unread_base[ix] as i64 + state.unread_delta[ix]).max(0) as u64;
        match folder {
            FolderKey::Folder(id) => folder_index(*id).map_or(0, count),
            FolderKey::Unified(role) => Self::members(*role).map(count).sum(),
        }
    }

    fn thread_keys(
        &self,
        scope: &ThreadScope,
        filter: &ThreadFilter,
        sort: ListSort,
    ) -> Result<ThreadKeys, SourceError> {
        let cacheable = filter.is_empty();
        let cache_key = (scope.clone(), sort);
        if cacheable
            && let Some(keys) = self
                .key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&cache_key)
        {
            return Ok(keys.clone());
        }
        let folders = Self::scope_folders(scope)?;
        let keys: ThreadKeys = {
            let state = self.read_state();
            let lists = folders
                .into_iter()
                .map(|folder| self.folder_keys(&state, folder, filter))
                .collect();
            let mut keys = Self::merge_by_date(lists, |id| self.thread_date(id));
            if sort.is_by_date() {
                if !sort.descending {
                    keys.reverse();
                }
            } else {
                let rows: Vec<ThreadRow> =
                    keys.iter().filter_map(|id| self.row(&state, *id)).collect();
                keys = sorted(&rows, sort, |row| SortRow {
                    id: row.id.0,
                    date: row.date,
                    value: Fields {
                        flags: row.flags,
                        attachment: row.has_attachments,
                        size: row.size,
                        date: row.date,
                        subject: row.subject.expose(),
                        sender: row.sender.expose(),
                        recipients: row.recipients.expose(),
                        correspondents: row.correspondents().expose(),
                        tags: &row.tags,
                        account: &row.account,
                    }
                    .sort_value(sort.by),
                })
                .into_iter()
                .map(|ix| rows[ix].id)
                .collect();
            }
            keys.into()
        };
        if cacheable {
            self.key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(cache_key, keys.clone());
        }
        Ok(keys)
    }

    fn thread_rows(&self, keys: &[ThreadId]) -> Vec<ThreadRow> {
        let state = self.read_state();
        keys.iter().filter_map(|id| self.row(&state, *id)).collect()
    }

    fn message_keys(
        &self,
        scope: &ThreadScope,
        filter: &ThreadFilter,
        sort: ListSort,
    ) -> Result<MessageKeys, SourceError> {
        let cacheable = filter.is_empty();
        let cache_key = (scope.clone(), sort);
        if cacheable
            && let Some(keys) = self
                .message_key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&cache_key)
        {
            return Ok(keys.clone());
        }
        let folders = Self::scope_folders(scope)?;
        let keys: MessageKeys = {
            let state = self.read_state();
            let lists = folders
                .into_iter()
                .map(|folder| self.folder_message_keys(&state, folder, filter))
                .collect();
            let mut keys = Self::merge_by_date(lists, |id| {
                decode_message(id).map_or(OffsetDateTime::UNIX_EPOCH, |(thread, index)| {
                    self.message_date(thread, index)
                })
            });
            if sort.is_by_date() {
                if !sort.descending {
                    keys.reverse();
                }
            } else {
                let rows: Vec<MessageRow> = keys
                    .iter()
                    .filter_map(|id| self.message_row(&state, *id))
                    .collect();
                keys = sorted(&rows, sort, |row| SortRow {
                    id: row.id.0,
                    date: row.date,
                    value: Fields {
                        flags: row.flags,
                        attachment: row.has_attachments,
                        size: row.size,
                        date: row.date,
                        subject: row.subject.expose(),
                        sender: row.sender.expose(),
                        recipients: row.recipients.expose(),
                        correspondents: row.correspondents().expose(),
                        tags: &row.tags,
                        account: &row.account,
                    }
                    .sort_value(sort.by),
                })
                .into_iter()
                .map(|ix| rows[ix].id)
                .collect();
            }
            keys.into()
        };
        if cacheable {
            self.message_key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(cache_key, keys.clone());
        }
        Ok(keys)
    }

    fn message_rows(&self, keys: &[MessageId]) -> Vec<MessageRow> {
        let state = self.read_state();
        keys.iter()
            .filter_map(|id| self.message_row(&state, *id))
            .collect()
    }

    fn thread_tree(&self, thread: ThreadId) -> Result<ThreadTree, SourceError> {
        let state = self.read_state();
        if decode(thread).is_none() || state.archived.contains(&thread) {
            return Err(SourceError::ThreadNotFound(thread));
        }
        Ok(self.tree(&state, thread).into())
    }

    fn message_counts(&self, threads: &[ThreadId]) -> Vec<u32> {
        let state = self.read_state();
        threads
            .iter()
            .map(|id| {
                if decode(*id).is_none() || state.archived.contains(id) {
                    0
                } else if state.archived_messages.is_empty() {
                    Self::message_count(*id)
                } else {
                    self.live_messages(&state, *id).count() as u32
                }
            })
            .collect()
    }

    fn find(
        &self,
        keys: &[ThreadId],
        from: Option<usize>,
        direction: Direction,
        predicate: ThreadPredicate,
    ) -> Option<usize> {
        let state = self.read_state();
        let matches = |ix: &usize| {
            let id = keys[*ix];
            match (predicate, decode(id)) {
                (ThreadPredicate::Unread, Some((folder, row))) => {
                    !state.archived.contains(&id)
                        && !self.flags(&state, id, folder, row).contains(Flags::SEEN)
                }
                (_, None) => false,
            }
        };
        match direction {
            Direction::Forward => (from.map_or(0, |ix| ix + 1)..keys.len()).find(matches),
            Direction::Backward => (0..from.unwrap_or(keys.len()).min(keys.len()))
                .rev()
                .find(matches),
        }
    }

    fn find_message(
        &self,
        keys: &[MessageId],
        from: Option<usize>,
        direction: Direction,
        predicate: ThreadPredicate,
    ) -> Option<usize> {
        let state = self.read_state();
        let matches = |ix: &usize| match (predicate, decode_message(keys[*ix])) {
            (ThreadPredicate::Unread, Some((thread, index))) => {
                !state.archived_messages.contains(&keys[*ix])
                    && !Self::message_flags(&state, thread, index).contains(Flags::SEEN)
            }
            (_, None) => false,
        };
        match direction {
            Direction::Forward => (from.map_or(0, |ix| ix + 1)..keys.len()).find(matches),
            Direction::Backward => (0..from.unwrap_or(keys.len()).min(keys.len()))
                .rev()
                .find(matches),
        }
    }

    fn message(&self, thread: ThreadId) -> Result<MessageDocument, SourceError> {
        let state = self.read_state();
        let newest = self
            .live_messages(&state, thread)
            .last()
            .filter(|_| !state.archived.contains(&thread))
            .ok_or(SourceError::ThreadNotFound(thread))?;
        self.document(&state, message_id(thread, newest))
            .map_err(|_| SourceError::ThreadNotFound(thread))
    }

    fn message_document(&self, message: MessageId) -> Result<MessageDocument, SourceError> {
        let state = self.read_state();
        self.document(&state, message)
    }

    fn set_flags(
        &self,
        threads: &[ThreadId],
        add: Flags,
        remove: Flags,
    ) -> Result<(), SourceError> {
        let mut state = self.write_state();
        self.checkpoint(&state);
        for id in threads {
            let (folder, row) = decode(*id).ok_or(SourceError::ThreadNotFound(*id))?;
            let before = self.flags(&state, *id, folder, row);
            let was_unread = !before.contains(Flags::SEEN);
            state.message_flags.remove(id);
            state.flags.insert(*id, (before | add) - remove);
            self.count_unread_change(&mut state, *id, was_unread);
        }
        Ok(())
    }

    fn set_message_flags(
        &self,
        messages: &[MessageId],
        add: Flags,
        remove: Flags,
    ) -> Result<(), SourceError> {
        let mut state = self.write_state();
        self.checkpoint(&state);
        for id in messages {
            let (thread, index) = decode_message(*id).ok_or(SourceError::MessageNotFound(*id))?;
            let (folder, row) = decode(thread).ok_or(SourceError::MessageNotFound(*id))?;
            let was_unread = !self
                .flags(&state, thread, folder, row)
                .contains(Flags::SEEN);
            let after = (Self::message_flags(&state, thread, index) | add) - remove;
            state
                .message_flags
                .entry(thread)
                .or_default()
                .insert(index, after);
            self.count_unread_change(&mut state, thread, was_unread);
        }
        Ok(())
    }

    fn archive(&self, threads: &[ThreadId]) -> Result<(), SourceError> {
        {
            let mut state = self.write_state();
            self.checkpoint(&state);
            for id in threads {
                let (folder, row) = decode(*id).ok_or(SourceError::ThreadNotFound(*id))?;
                let unread = !self.flags(&state, *id, folder, row).contains(Flags::SEEN);
                if state.archived.insert(*id) && unread {
                    state.unread_delta[folder] -= 1;
                }
            }
        }
        self.invalidate_keys();
        Ok(())
    }

    fn archive_messages(&self, messages: &[MessageId]) -> Result<(), SourceError> {
        {
            let mut state = self.write_state();
            self.checkpoint(&state);
            for id in messages {
                let (thread, _) = decode_message(*id).ok_or(SourceError::MessageNotFound(*id))?;
                let (folder, row) = decode(thread).ok_or(SourceError::MessageNotFound(*id))?;
                let was_unread = !self
                    .flags(&state, thread, folder, row)
                    .contains(Flags::SEEN);
                state.archived_messages.insert(*id);
                if self.live_messages(&state, thread).next().is_none() {
                    // The last message went: the thread goes with it.
                    if state.archived.insert(thread) && was_unread {
                        state.unread_delta[folder] -= 1;
                    }
                } else {
                    self.count_unread_change(&mut state, thread, was_unread);
                }
            }
        }
        self.invalidate_keys();
        Ok(())
    }

    fn undo(&self) -> Result<bool, SourceError> {
        let Some(before) = self
            .undo
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        else {
            return Ok(false);
        };
        *self.write_state() = before;
        self.invalidate_keys();
        Ok(true)
    }

    fn thread_account(&self, thread: ThreadId) -> Option<AccountId> {
        decode(thread).map(|(folder, _)| Self::account_id(folder))
    }

    fn message_account(&self, message: MessageId) -> Option<AccountId> {
        decode_message(message).and_then(|(thread, _)| self.thread_account(thread))
    }

    fn move_threads(
        &self,
        threads: &[ThreadId],
        to: FolderId,
        _from: Option<FolderId>,
    ) -> Result<(), SourceError> {
        for id in threads {
            let account = self
                .thread_account(*id)
                .ok_or(SourceError::ThreadNotFound(*id))?;
            Self::check_target(account, to)?;
        }
        self.archive(threads)?;
        let mut state = self.write_state();
        for id in threads {
            state.moves.insert(MoveItem::Thread(*id), to);
        }
        Ok(())
    }

    fn move_messages(
        &self,
        messages: &[MessageId],
        to: FolderId,
        _from: Option<FolderId>,
    ) -> Result<(), SourceError> {
        for id in messages {
            let account = self
                .message_account(*id)
                .ok_or(SourceError::MessageNotFound(*id))?;
            Self::check_target(account, to)?;
        }
        self.archive_messages(messages)?;
        let mut state = self.write_state();
        for id in messages {
            state.moves.insert(MoveItem::Message(*id), to);
        }
        Ok(())
    }

    fn status(&self) -> SourceStatus {
        SourceStatus {
            sync: SyncState::Idle,
            outbox: 0,
            online: true,
        }
    }
}

/// A shared fake source, as the window takes it.
pub fn shared() -> Arc<dyn MailSource> {
    Arc::new(FakeMailSource::new())
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn source() -> FakeMailSource {
        FakeMailSource::with_now(datetime!(2026-10-22 10:12 UTC))
    }

    fn all(source: &FakeMailSource, key: FolderKey) -> ThreadKeys {
        source
            .thread_keys(
                &ThreadScope::Folder(key),
                &ThreadFilter::default(),
                ListSort::NEWEST_FIRST,
            )
            .unwrap()
    }

    #[test]
    fn tree_has_sections_and_accounts() {
        let tree = source().folder_tree();
        assert_eq!(tree.accounts.len(), 5);
        assert_eq!(tree.unified.len(), 6);
        assert_eq!(tree.favorites.len(), 3);
        assert_eq!(tree.accounts[0].name.as_ref(), "Gmail (personal)");
        assert_eq!(tree.accounts[0].folders[0].role, Some(Inbox));
        // Subfolders hang under their parent.
        let ms = &tree.accounts[2];
        let customers = ms
            .folders
            .iter()
            .find(|f| f.name.as_ref() == "Customers")
            .unwrap();
        assert_eq!(customers.children[0].name.as_ref(), "Acme");
        assert!(tree.find(&FakeMailSource::large_folder()).is_some());
    }

    #[test]
    fn large_folder_has_a_million_sorted_threads() {
        let source = source();
        let keys = all(&source, FakeMailSource::large_folder());
        assert_eq!(keys.len(), LARGE_FOLDER_THREADS as usize);
        let rows = source.thread_rows(&keys[500_000..500_050]);
        assert_eq!(rows.len(), 50);
        assert!(rows.windows(2).all(|w| w[0].date > w[1].date));
        // Deterministic: same id, same row.
        assert_eq!(
            source.thread_rows(&keys[7..8]),
            source.thread_rows(&keys[7..8])
        );
    }

    #[test]
    fn unified_inbox_merges_accounts_by_date() {
        let source = source();
        let keys = all(&source, FolderKey::Unified(Inbox));
        let expected: u32 = FOLDERS
            .iter()
            .filter(|f| f.role == Some(Inbox))
            .map(|f| f.threads)
            .sum();
        assert_eq!(keys.len(), expected as usize);
        let rows = source.thread_rows(&keys[..200]);
        assert!(rows.windows(2).all(|w| w[0].date >= w[1].date));
        let folders: HashSet<_> = keys[..200]
            .iter()
            .filter_map(|id| decode(*id))
            .map(|(f, _)| f)
            .collect();
        assert!(folders.len() > 1, "unified inbox mixes accounts");
    }

    #[test]
    fn reading_and_archiving_update_counts_and_keys() {
        let source = source();
        let inbox = FolderKey::Folder(folder_id(0));
        let keys = all(&source, inbox.clone());
        let unread_before = source.unread_count(&inbox);
        let unified_before = source.unread_count(&FolderKey::Unified(Inbox));
        let first_unread = source
            .find(&keys, None, Direction::Forward, ThreadPredicate::Unread)
            .unwrap();
        let id = keys[first_unread];

        source
            .set_flags(&[id], Flags::SEEN, Flags::empty())
            .unwrap();
        assert_eq!(source.unread_count(&inbox), unread_before - 1);
        assert_eq!(
            source.unread_count(&FolderKey::Unified(Inbox)),
            unified_before - 1
        );
        assert!(!source.thread_rows(&[id])[0].is_unread());
        let next = source
            .find(&keys, None, Direction::Forward, ThreadPredicate::Unread)
            .unwrap();
        assert!(next > first_unread);

        source.archive(&[keys[0]]).unwrap();
        let after = all(&source, inbox);
        assert_eq!(after.len(), keys.len() - 1);
        assert!(!after.contains(&keys[0]));
        assert!(source.thread_rows(&[keys[0]]).is_empty());
    }

    #[test]
    fn filters_by_flags_and_text() {
        let source = source();
        let scope = ThreadScope::Folder(FolderKey::Folder(folder_id(0)));
        let unread = source
            .thread_keys(
                &scope,
                &ThreadFilter {
                    unread: true,
                    ..ThreadFilter::default()
                },
                ListSort::NEWEST_FIRST,
            )
            .unwrap();
        assert_eq!(
            unread.len() as u64,
            source.unread_count(&FolderKey::Folder(folder_id(0)))
        );
        let invoices = source
            .thread_keys(
                &scope,
                &ThreadFilter {
                    text: Some("INVOICE".into()),
                    ..ThreadFilter::default()
                },
                ListSort::NEWEST_FIRST,
            )
            .unwrap();
        assert!(!invoices.is_empty());
        for row in source.thread_rows(&invoices) {
            assert!(row.subject.expose().to_lowercase().contains("invoice"));
        }
    }

    #[test]
    fn default_find_matches_the_fast_path() {
        struct Slow(FakeMailSource);
        impl MailSource for Slow {
            fn folder_tree(&self) -> FolderTree {
                self.0.folder_tree()
            }
            fn unread_count(&self, folder: &FolderKey) -> u64 {
                self.0.unread_count(folder)
            }
            fn thread_keys(
                &self,
                scope: &ThreadScope,
                filter: &ThreadFilter,
                sort: ListSort,
            ) -> Result<ThreadKeys, SourceError> {
                self.0.thread_keys(scope, filter, sort)
            }
            fn thread_rows(&self, keys: &[ThreadId]) -> Vec<ThreadRow> {
                self.0.thread_rows(keys)
            }
            fn message_keys(
                &self,
                scope: &ThreadScope,
                filter: &ThreadFilter,
                sort: ListSort,
            ) -> Result<MessageKeys, SourceError> {
                self.0.message_keys(scope, filter, sort)
            }
            fn message_rows(&self, keys: &[MessageId]) -> Vec<MessageRow> {
                self.0.message_rows(keys)
            }
            fn thread_tree(&self, thread: ThreadId) -> Result<ThreadTree, SourceError> {
                self.0.thread_tree(thread)
            }
            fn message(&self, thread: ThreadId) -> Result<MessageDocument, SourceError> {
                self.0.message(thread)
            }
            fn message_document(&self, id: MessageId) -> Result<MessageDocument, SourceError> {
                self.0.message_document(id)
            }
            fn set_flags(&self, t: &[ThreadId], a: Flags, r: Flags) -> Result<(), SourceError> {
                self.0.set_flags(t, a, r)
            }
            fn set_message_flags(
                &self,
                m: &[MessageId],
                a: Flags,
                r: Flags,
            ) -> Result<(), SourceError> {
                self.0.set_message_flags(m, a, r)
            }
            fn archive(&self, threads: &[ThreadId]) -> Result<(), SourceError> {
                self.0.archive(threads)
            }
            fn archive_messages(&self, m: &[MessageId]) -> Result<(), SourceError> {
                self.0.archive_messages(m)
            }
            fn undo(&self) -> Result<bool, SourceError> {
                self.0.undo()
            }
            fn thread_account(&self, thread: ThreadId) -> Option<AccountId> {
                self.0.thread_account(thread)
            }
            fn message_account(&self, message: MessageId) -> Option<AccountId> {
                self.0.message_account(message)
            }
            fn move_threads(
                &self,
                t: &[ThreadId],
                to: FolderId,
                from: Option<FolderId>,
            ) -> Result<(), SourceError> {
                self.0.move_threads(t, to, from)
            }
            fn move_messages(
                &self,
                m: &[MessageId],
                to: FolderId,
                from: Option<FolderId>,
            ) -> Result<(), SourceError> {
                self.0.move_messages(m, to, from)
            }
            fn set_favorite(&self, folder: FolderId, favorite: bool) -> Result<(), SourceError> {
                self.0.set_favorite(folder, favorite)
            }
            fn status(&self) -> SourceStatus {
                self.0.status()
            }
        }
        let slow = Slow(source());
        let keys = all(&slow.0, FolderKey::Folder(folder_id(0)));
        let messages = slow
            .message_keys(
                &ThreadScope::Folder(FolderKey::Folder(folder_id(0))),
                &ThreadFilter::default(),
                ListSort::NEWEST_FIRST,
            )
            .unwrap();
        for from in [None, Some(0), Some(100), Some(keys.len() - 1)] {
            for direction in [Direction::Forward, Direction::Backward] {
                assert_eq!(
                    slow.find(&keys, from, direction, ThreadPredicate::Unread),
                    slow.0.find(&keys, from, direction, ThreadPredicate::Unread),
                    "{from:?} {direction:?}"
                );
                assert_eq!(
                    slow.find_message(&messages, from, direction, ThreadPredicate::Unread),
                    slow.0
                        .find_message(&messages, from, direction, ThreadPredicate::Unread),
                    "{from:?} {direction:?}"
                );
            }
        }
        // The default counts match the fast ones.
        assert_eq!(
            slow.message_counts(&keys[..300]),
            slow.0.message_counts(&keys[..300])
        );
    }

    #[test]
    fn messages_render_html() {
        let source = source();
        let keys = all(&source, FolderKey::Unified(Inbox));
        let doc = source.message(keys[0]).unwrap();
        let MessageBody::Html(html) = &doc.body else {
            panic!("expected html");
        };
        assert!(html.expose().starts_with("<p>Hi "));
        assert_eq!(doc.to.expose().len(), 1);
        assert!(source.message(ThreadId(0)).is_err());
    }

    #[test]
    fn threads_have_reply_trees_and_messages() {
        let source = source();
        let keys = all(&source, FolderKey::Folder(folder_id(0)));
        let rows = source.thread_rows(&keys[..200]);
        let multi = rows.iter().find(|r| r.message_count >= 4).unwrap();
        let tree = source.thread_tree(multi.id).unwrap();
        assert_eq!(tree.len(), multi.message_count as usize);
        assert_eq!(tree[0].depth, 0);
        assert!(tree.iter().skip(1).all(|n| n.parent.is_some()));
        // Depth first: a node's parent comes before it, one level up.
        for (ix, node) in tree.iter().enumerate() {
            if let Some(parent) = node.parent {
                let p = tree.iter().position(|n| n.message == parent).unwrap();
                assert!(p < ix);
                assert_eq!(tree[p].depth + 1, node.depth);
            }
        }
        let ids: Vec<MessageId> = tree.iter().map(|n| n.message).collect();
        let messages = source.message_rows(&ids);
        assert_eq!(messages.len(), ids.len());
        assert!(messages.iter().all(|m| m.thread == multi.id));
        assert!(
            messages.iter().any(|m| m.outgoing),
            "you replied in the thread"
        );
        let doc = source.message_document(ids[1]).unwrap();
        assert_eq!(doc.message, ids[1]);
        assert_eq!(doc.thread, multi.id);
        assert_eq!(source.message_counts(&[multi.id])[0], multi.message_count);
    }

    #[test]
    fn unthreaded_keys_list_every_message_by_date() {
        let source = source();
        let scope = ThreadScope::Folder(FolderKey::Folder(folder_id(0)));
        let threads = all(&source, FolderKey::Folder(folder_id(0)));
        let messages = source
            .message_keys(&scope, &ThreadFilter::default(), ListSort::NEWEST_FIRST)
            .unwrap();
        let total: u32 = source.message_counts(&threads).iter().sum();
        assert_eq!(messages.len(), total as usize);
        let rows = source.message_rows(&messages[..500]);
        assert!(rows.windows(2).all(|w| w[0].date >= w[1].date));
        let oldest = source
            .message_keys(&scope, &ThreadFilter::default(), ListSort::OLDEST_FIRST)
            .unwrap();
        assert_eq!(oldest.first(), messages.last());
        // Sent mail shows its recipients as correspondents.
        let sent = source
            .message_keys(
                &ThreadScope::Folder(FolderKey::Folder(folder_id(2))),
                &ThreadFilter::default(),
                ListSort::NEWEST_FIRST,
            )
            .unwrap();
        let newest = &source.message_rows(&sent[..1])[0];
        assert!(newest.outgoing);
        assert_eq!(newest.correspondents(), &newest.recipients);
    }

    #[test]
    fn message_flags_roll_up_to_the_thread() {
        let source = source();
        let inbox = FolderKey::Folder(folder_id(0));
        let keys = all(&source, inbox.clone());
        let thread = source
            .thread_rows(&keys[..200])
            .into_iter()
            .find(|r| r.message_count >= 3 && !r.is_unread())
            .unwrap();
        let tree = source.thread_tree(thread.id).unwrap();
        let unread_before = source.unread_count(&inbox);
        source
            .set_message_flags(&[tree[0].message], Flags::FLAGGED, Flags::SEEN)
            .unwrap();
        let row = &source.thread_rows(&[thread.id])[0];
        assert!(row.is_unread() && row.is_starred());
        assert_eq!(source.unread_count(&inbox), unread_before + 1);
        // Archiving one message keeps the thread with one message less.
        source.archive_messages(&[tree[0].message]).unwrap();
        let row = &source.thread_rows(&[thread.id])[0];
        assert_eq!(row.message_count, thread.message_count - 1);
        assert!(!row.is_unread());
        assert_eq!(source.unread_count(&inbox), unread_before);
        assert_eq!(source.thread_tree(thread.id).unwrap().len(), tree.len() - 1);
    }
}

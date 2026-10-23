//! A [`MailSource`] with synthetic, deterministic data.
//!
//! It backs demos (`MAZEMAIL_DEMO=1`), tests and the S1 measurements, which run
//! against: five accounts, unified folders, favorites, and one folder with 1,000,000
//! threads. Every row is computed from its thread id with a hash, so nothing is
//! stored per message: the 1M folder costs no memory beyond its key list, the same
//! way the real thread list only holds keys.
//!
//! Changes (read, starred, archived) are kept in memory. Archiving hides a thread
//! from its folder; the fake doesn't add it to an Archives folder.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use gpui::SharedString;
use mazemail_types::{
    AccountId, Address, Flags, FolderId, FolderRole, MessageId, Sensitive, ThreadId,
};
use time::{Duration, OffsetDateTime};

use crate::source::{
    AccountEntry, AccountKind, AttachmentInfo, Direction, FolderEntry, FolderKey, FolderTree,
    MailSource, MessageBody, MessageDocument, SourceError, SourceStatus, SyncState, ThreadFilter,
    ThreadKeys, ThreadPredicate, ThreadRow, ThreadScope, Verdict, Verification,
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
        name: "Local Folders",
        kind: AccountKind::Local,
        address: "philip@localhost",
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
    // 28..=29: Local Folders
    folder(4, None, "Old mail", None, 25_000, 15 * YEAR),
    folder(4, None, "Trash", Some(Trash), 0, DAY),
];

/// Favorites, by folder index.
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

fn folder_id(folder: usize) -> FolderId {
    FolderId(folder as i64 + 1)
}

fn folder_index(id: FolderId) -> Option<usize> {
    usize::try_from(id.0 - 1)
        .ok()
        .filter(|ix| *ix < FOLDERS.len())
}

/// Mutable state: changes made through the UI.
#[derive(Default)]
struct State {
    /// Flags that differ from the generated ones.
    flags: HashMap<ThreadId, Flags>,
    archived: HashSet<ThreadId>,
    /// Unread count changes per folder, relative to the generated counts.
    unread_delta: Vec<i64>,
}

/// Synthetic mail for development and benchmarks. See the module docs.
pub struct FakeMailSource {
    now: OffsetDateTime,
    unread_base: Vec<u64>,
    state: RwLock<State>,
    /// Unfiltered key lists, built on first use.
    key_cache: Mutex<HashMap<ThreadScope, ThreadKeys>>,
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
                ..State::default()
            }),
            key_cache: Mutex::new(HashMap::new()),
        }
    }

    /// The key of the 1,000,000-thread folder.
    pub fn large_folder() -> FolderKey {
        FolderKey::Folder(folder_id(3))
    }

    fn hash(folder: usize, row: u32) -> u64 {
        mix(((folder as u64) << 32) | u64::from(row))
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

    fn flags(&self, state: &State, id: ThreadId, folder: usize, row: u32) -> Flags {
        state
            .flags
            .get(&id)
            .copied()
            .unwrap_or_else(|| Self::generated_flags(folder, row))
    }

    fn date(&self, folder: usize, row: u32) -> OffsetDateTime {
        let spec = &FOLDERS[folder];
        let cadence = (spec.span_secs / i64::from(spec.threads.max(1))).max(30);
        let jitter = (Self::hash(folder, row) >> 3) as i64 % cadence;
        // Strictly decreasing with the row, so key order is date order.
        self.now - Duration::seconds(i64::from(row) * cadence + jitter + 240)
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

    fn row(&self, state: &State, id: ThreadId) -> Option<ThreadRow> {
        let (folder, row) = decode(id)?;
        if state.archived.contains(&id) {
            return None;
        }
        let spec = &FOLDERS[folder];
        let h = Self::hash(folder, row);
        let date = self.date(folder, row);
        let message_count = if (h >> 44) % 100 < 30 {
            2 + ((h >> 50) % 7) as u32
        } else {
            1
        };
        let mut subject = Self::fill(pick(SUBJECTS, h >> 37), h, date);
        if message_count > 1 {
            subject.insert_str(0, "Re: ");
        }
        let (name, _) = pick(SENDERS, h);
        let sender = match spec.role {
            Some(Sent | Drafts) => format!("To: {name}"),
            _ => name.to_owned(),
        };
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
            sender: Sensitive::new(sender.into()),
            date,
            flags: self.flags(state, id, folder, row),
            has_attachments,
            message_count,
            size,
        })
    }

    fn read_state(&self) -> std::sync::RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_state(&self) -> std::sync::RwLockWriteGuard<'_, State> {
        self.state.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn invalidate_keys(&self) {
        self.key_cache
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

    /// Merges per-folder key lists (each newest first) into one, newest first.
    fn merge_by_date(&self, lists: Vec<Vec<ThreadId>>) -> Vec<ThreadId> {
        let mut lists: Vec<_> = lists.into_iter().filter(|l| !l.is_empty()).collect();
        if lists.len() <= 1 {
            return lists.pop().unwrap_or_default();
        }
        let total = lists.iter().map(Vec::len).sum();
        let mut out = Vec::with_capacity(total);
        let mut cursors = vec![0usize; lists.len()];
        let date_of =
            |id: ThreadId| decode(id).map_or(OffsetDateTime::UNIX_EPOCH, |(f, r)| self.date(f, r));
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

    fn body(&self, row: &ThreadRow, folder: usize, r: u32, recipient: &str) -> String {
        let h = Self::hash(folder, r);
        let (name, _) = pick(SENDERS, h);
        let first_name = name.split_whitespace().next().unwrap_or(name);
        let greeting = recipient.split('@').next().unwrap_or("there");
        let greeting = greeting
            .split('.')
            .next()
            .map(|g| {
                let mut chars = g.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect::<String>())
                    .unwrap_or_default()
            })
            .unwrap_or_default();
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
        if row.subject.expose().starts_with("Invoice") {
            html.push_str(
                "<table><tr><th>Item</th><th>Amount</th></tr>\
                 <tr><td>Cloud servers</td><td>€ 182.40</td></tr>\
                 <tr><td>Backups</td><td>€ 12.00</td></tr>\
                 <tr><td><b>Total</b></td><td><b>€ 194.40</b></td></tr></table>",
            );
        }
        html.push_str(&format!("<p>Best,<br>{first_name}</p>"));
        if row.message_count > 1 {
            html.push_str(&format!(
                "<blockquote><p>On {}, {greeting} wrote:</p><p>{}</p></blockquote>",
                (row.date - Duration::days(1)).date(),
                sentence(h >> 27)
            ));
        }
        html
    }
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
        let favorites = FAVORITES
            .iter()
            .map(|ix| {
                let mut entry = self.folder_entry(*ix);
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
    ) -> Result<ThreadKeys, SourceError> {
        let cacheable = filter.is_empty();
        if cacheable
            && let Some(keys) = self
                .key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(scope)
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
            self.merge_by_date(lists).into()
        };
        if cacheable {
            self.key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(scope.clone(), keys.clone());
        }
        Ok(keys)
    }

    fn thread_rows(&self, keys: &[ThreadId]) -> Vec<ThreadRow> {
        let state = self.read_state();
        keys.iter().filter_map(|id| self.row(&state, *id)).collect()
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

    fn message(&self, thread: ThreadId) -> Result<MessageDocument, SourceError> {
        let state = self.read_state();
        let row = self
            .row(&state, thread)
            .ok_or(SourceError::ThreadNotFound(thread))?;
        let (folder, r) = decode(thread).ok_or(SourceError::ThreadNotFound(thread))?;
        let account = &ACCOUNTS[FOLDERS[folder].account];
        let h = Self::hash(folder, r);
        let (name, email) = pick(SENDERS, h);
        let me = Address::new(Some("Philip"), account.address);
        let them = Address::new(Some(name), email);
        let (from, to) = match FOLDERS[folder].role {
            Some(Sent | Drafts) => (me, vec![them]),
            _ => (them, vec![me]),
        };
        let cc = if (h >> 45) % 4 == 0 {
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
                size: row.size.saturating_sub(2_000),
            }]
        } else {
            Vec::new()
        };
        let body = self.body(&row, folder, r, account.address);
        Ok(MessageDocument {
            thread,
            message: MessageId(thread.0),
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
        })
    }

    fn set_flags(
        &self,
        threads: &[ThreadId],
        add: Flags,
        remove: Flags,
    ) -> Result<(), SourceError> {
        let mut state = self.write_state();
        for id in threads {
            let (folder, row) = decode(*id).ok_or(SourceError::ThreadNotFound(*id))?;
            let before = self.flags(&state, *id, folder, row);
            let after = (before | add) - remove;
            let was_unread = !before.contains(Flags::SEEN);
            let is_unread = !after.contains(Flags::SEEN);
            if was_unread != is_unread && !state.archived.contains(id) {
                state.unread_delta[folder] += if is_unread { 1 } else { -1 };
            }
            state.flags.insert(*id, after);
        }
        Ok(())
    }

    fn archive(&self, threads: &[ThreadId]) -> Result<(), SourceError> {
        {
            let mut state = self.write_state();
            for id in threads {
                let (folder, row) = decode(*id).ok_or(SourceError::ThreadNotFound(*id))?;
                if state.archived.insert(*id)
                    && !self.flags(&state, *id, folder, row).contains(Flags::SEEN)
                {
                    state.unread_delta[folder] -= 1;
                }
            }
        }
        self.invalidate_keys();
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
            .thread_keys(&ThreadScope::Folder(key), &ThreadFilter::default())
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
            ) -> Result<ThreadKeys, SourceError> {
                self.0.thread_keys(scope, filter)
            }
            fn thread_rows(&self, keys: &[ThreadId]) -> Vec<ThreadRow> {
                self.0.thread_rows(keys)
            }
            fn message(&self, thread: ThreadId) -> Result<MessageDocument, SourceError> {
                self.0.message(thread)
            }
            fn set_flags(&self, t: &[ThreadId], a: Flags, r: Flags) -> Result<(), SourceError> {
                self.0.set_flags(t, a, r)
            }
            fn archive(&self, threads: &[ThreadId]) -> Result<(), SourceError> {
                self.0.archive(threads)
            }
            fn status(&self) -> SourceStatus {
                self.0.status()
            }
        }
        let slow = Slow(source());
        let keys = all(&slow.0, FolderKey::Folder(folder_id(0)));
        for from in [None, Some(0), Some(100), Some(keys.len() - 1)] {
            for direction in [Direction::Forward, Direction::Backward] {
                assert_eq!(
                    slow.find(&keys, from, direction, ThreadPredicate::Unread),
                    slow.0.find(&keys, from, direction, ThreadPredicate::Unread),
                    "{from:?} {direction:?}"
                );
            }
        }
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
}

//! `cargo xtask seed-demo`: a realistic synthetic mailbox for demos, screenshots and
//! end-to-end tests.
//!
//! The mail is generated as mbox text (deterministic for a given `--seed`), read with
//! `import`'s mbox reader (mboxrd parsing, `X-Mozilla-Status` flags) and stored the way
//! synced mail is: `ingest` (parsing, threading, encrypted blobs) with server UIDs and
//! the core's search index. Two IMAP accounts without a server, "Personal" and "Work"
//! (they never sync), get an Inbox, Archive, Sent and a Newsletters folder; about a third of the messages are replies
//! (threads), some carry attachments, the newsletters are HTML with links and remote
//! images, and a share is unread or flagged.
//!
//! The address book gets addresses you wrote to (the collected book) and an imported
//! "Personal Address Book" with phone numbers and organizations, overlapping, so the
//! Address Book space shows people with two labels.
//!
//! The data directory uses the development file key store
//! (`MAZEMAIL_INSECURE_DEV_KEYSTORE=1`), never the keychain, and the settings file
//! shares both accounts with agents, so the local API works on the demo data at once.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use import::mbox::{Escaping, MboxReader};
use ingest::{Ingestor, MessageMeta, RawItem};
use mazemail_core::{CoreConfig, CoreHandle};
use mazemail_types::FolderRole;
use store::{AccountKind, NewAccount, NewFolder, NewLocation};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc2822;

/// The key file `mazemail` reads with `MAZEMAIL_INSECURE_DEV_KEYSTORE=1`
/// (`mazemail::keys::DEV_KEY_FILE`; keep the two equal).
pub(crate) const DEV_KEY_FILE: &str = "insecure-dev-master-key";

/// The demo's own address.
const ME: (&str, &str) = ("Philip Miglinci", "philip@maze-demo.example");

/// Options of `seed-demo`.
#[derive(Debug, Clone)]
pub(crate) struct SeedOptions {
    pub(crate) data_dir: PathBuf,
    pub(crate) config_dir: PathBuf,
    pub(crate) messages: usize,
    pub(crate) seed: u64,
}

/// A tiny deterministic generator (SplitMix64): no dependency, same mail every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n.max(1) as u64)).unwrap_or(0)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// `true` with probability `percent`/100.
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

const PEOPLE: &[(&str, &str)] = &[
    ("Anna Berger", "anna.berger@northwind.example"),
    ("Ben Okafor", "ben@okafor-design.example"),
    ("Clara Wu", "clara.wu@lumen-labs.example"),
    ("David Moreau", "d.moreau@atelier-moreau.example"),
    ("Elif Kaya", "elif@kaya.example"),
    ("Felix Hartmann", "felix.hartmann@northwind.example"),
    ("Grace Liu", "grace@liu-family.example"),
    ("Hannah Schmidt", "hannah.schmidt@stadtwerk.example"),
    ("Isaac Novak", "isaac@novak.example"),
    ("Julia Rossi", "julia.rossi@lumen-labs.example"),
    ("Kenji Tanaka", "kenji@tanaka.example"),
    ("Lena Fischer", "lena@fischer-legal.example"),
    ("Mateo García", "mateo@garcia-studio.example"),
    ("Nora Lindqvist", "nora.lindqvist@northwind.example"),
    ("Omar Haddad", "omar@haddad.example"),
    ("Priya Natarajan", "priya@natarajan.example"),
];

const COMPANIES: &[&str] = &[
    "Northwind",
    "Lumen Labs",
    "Stadtwerk",
    "Atelier Moreau",
    "Kaya & Co",
];
const TOPICS: &[&str] = &[
    "search latency",
    "the onboarding flow",
    "offline sync",
    "the Q4 roadmap",
    "the pricing page",
    "accessibility audit",
    "the mobile beta",
    "database migration",
];
const PLACES: &[&str] = &[
    "Lisbon",
    "Vienna",
    "Kyoto",
    "the lake",
    "Bergen",
    "Marseille",
];
const DAYS: &[&str] = &["Friday", "Saturday", "Sunday", "Tuesday", "Thursday"];
const BOOKS: &[&str] = &["The Overstory", "Piranesi", "Klara and the Sun", "Stoner"];
const ATTACHMENTS: &[(&str, &str)] = &[
    ("budget-q4.pdf", "application/pdf"),
    ("agenda.pdf", "application/pdf"),
    ("contract-draft.pdf", "application/pdf"),
    ("photo-lake.jpg", "image/jpeg"),
    ("invoice-2026-117.pdf", "application/pdf"),
    ("slides.pdf", "application/pdf"),
];
const SENTENCES: &[&str] = &[
    "Thanks for the quick turnaround on this.",
    "I went through the numbers again and they look solid.",
    "Could you take a look before our call tomorrow?",
    "Let me know if anything is unclear.",
    "We should decide by the end of the week.",
    "I added my notes to the shared document.",
    "The team agreed to move forward with the second option.",
    "Happy to jump on a call if that's easier.",
    "I'll be offline on Monday, so please loop in Felix.",
    "This is mostly done; only the last section needs another pass.",
    "Attached is the latest version for your review.",
    "No rush, but it would help to have it before the offsite.",
];

/// A message replies can refer to: Message-ID, subject and sender.
type Earlier = (String, String, (&'static str, &'static str));

/// One generated message.
struct Generated {
    folder: usize,
    text: String,
}

/// The demo's folders: name, role, account (0: Personal, 1: Work).
const FOLDERS: &[(&str, Option<FolderRole>, usize)] = &[
    ("Inbox", Some(FolderRole::Inbox), 0),
    ("Archive", Some(FolderRole::Archive), 0),
    ("Sent", Some(FolderRole::Sent), 0),
    ("Newsletters", None, 0),
    ("Inbox", Some(FolderRole::Inbox), 1),
    ("Archive", Some(FolderRole::Archive), 1),
    ("Sent", Some(FolderRole::Sent), 1),
];
const ACCOUNTS: &[&str] = &["Personal", "Work"];

fn subject(rng: &mut Rng, work: bool, n: usize) -> String {
    if work {
        match rng.below(7) {
            0 => format!("Q4 planning: {}", rng.pick(TOPICS)),
            1 => format!("Design review for {}", rng.pick(TOPICS)),
            2 => format!("Invoice {} from {}", 2_026_000 + n, rng.pick(COMPANIES)),
            3 => format!("Contract renewal – {}", rng.pick(COMPANIES)),
            4 => format!("Offsite agenda ({})", rng.pick(PLACES)),
            5 => format!("Incident {}: {}", 400 + n % 97, rng.pick(TOPICS)),
            _ => format!("Notes from today's sync on {}", rng.pick(TOPICS)),
        }
    } else {
        match rng.below(6) {
            0 => format!("Dinner on {}?", rng.pick(DAYS)),
            1 => format!("Photos from {}", rng.pick(PLACES)),
            2 => format!("Flight to {} confirmed", rng.pick(PLACES)),
            3 => format!("Your order #{} has shipped", 58_000 + n * 7),
            4 => format!("Book club: {}", rng.pick(BOOKS)),
            _ => format!("Climbing on {}", rng.pick(DAYS)),
        }
    }
}

fn paragraphs(rng: &mut Rng, greeting: &str, sender: &str) -> String {
    let mut body = format!("Hi {greeting},\n\n");
    for _ in 0..1 + rng.below(3) {
        // Distinct sentences: a consecutive run from a random start.
        let start = rng.below(SENTENCES.len());
        let sentences: Vec<&str> = (0..2 + rng.below(3))
            .map(|i| SENTENCES[(start + i) % SENTENCES.len()])
            .collect();
        body.push_str(&sentences.join(" "));
        body.push_str("\n\n");
    }
    let first = sender.split_whitespace().next().unwrap_or(sender);
    let _ = write!(body, "Best,\n{first}\n");
    body
}

fn date(at: i64) -> String {
    OffsetDateTime::from_unix_timestamp(at)
        .ok()
        .and_then(|d| d.format(&Rfc2822).ok())
        .unwrap_or_else(|| "Thu, 01 Jan 1970 00:00:00 +0000".to_owned())
}

/// The mbox `From ` line date (asctime, UTC).
fn asctime(at: i64) -> String {
    let format = time::macros::format_description!(
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
    );
    OffsetDateTime::from_unix_timestamp(at)
        .ok()
        .and_then(|d| d.format(&format).ok())
        .unwrap_or_else(|| "Thu Jan  1 00:00:00 1970".to_owned())
}

/// Base64 without line breaks longer than 76 characters.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for (ix, chunk) in data.chunks(3).enumerate() {
        if ix > 0 && ix % 19 == 0 {
            out.push_str("\r\n");
        }
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

struct Header<'a> {
    id: &'a str,
    from: (&'a str, &'a str),
    to: (&'a str, &'a str),
    subject: &'a str,
    at: i64,
    reply_to: Option<&'a str>,
    status: u32,
    list: Option<&'a str>,
}

fn headers(h: &Header<'_>) -> String {
    let mut out = format!(
        "From {} {}\nX-Mozilla-Status: {:04x}\nMessage-ID: <{}>\nDate: {}\nFrom: {} <{}>\nTo: {} <{}>\nSubject: {}\n",
        h.from.1,
        asctime(h.at),
        h.status,
        h.id,
        date(h.at),
        h.from.0,
        h.from.1,
        h.to.0,
        h.to.1,
        h.subject
    );
    if let Some(parent) = h.reply_to {
        let _ = write!(out, "In-Reply-To: <{parent}>\nReferences: <{parent}>\n");
    }
    if let Some(list) = h.list {
        let _ = write!(
            out,
            "List-Id: <{list}>\nList-Unsubscribe: <https://{list}/unsubscribe>\n"
        );
    }
    out.push_str("MIME-Version: 1.0\n");
    out
}

/// mboxrd-escapes `From ` lines of a body.
fn escape_body(body: &str) -> String {
    body.lines()
        .map(|line| {
            let quotes = line.chars().take_while(|c| *c == '>').count();
            if line[quotes..].starts_with("From ") {
                format!(">{line}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect()
}

fn newsletter(rng: &mut Rng, n: usize, at: i64) -> String {
    let (name, list) = *rng.pick(&[
        ("This Week in Rust", "this-week-in-rust.example"),
        ("Northwind Monthly", "news.northwind.example"),
        ("The Lumen Digest", "digest.lumen-labs.example"),
    ]);
    let subject = format!("{name} #{}", 300 + n);
    let mut html = format!(
        "<html><body><h1>{name}</h1><img src=\"https://{list}/banner-{n}.png\" alt=\"{name}\" \
         width=\"600\" height=\"120\">"
    );
    for i in 0..3 + rng.below(3) {
        let topic = rng.pick(TOPICS);
        let _ = write!(
            html,
            "<h2>Story {}: rethinking {topic}</h2><p>{} {} <a href=\"https://{list}/story/{n}-{i}\">\
             Read the full story</a>.</p>",
            i + 1,
            rng.pick(SENTENCES),
            rng.pick(SENTENCES)
        );
    }
    let _ = write!(
        html,
        "<p><small>You receive this because you subscribed. <a href=\"https://{list}/unsubscribe\">\
         Unsubscribe</a>.</small></p><img src=\"https://track.{list}/open/{n}.gif\" width=\"1\" \
         height=\"1\"></body></html>"
    );
    let status = if rng.chance(60) { 0x0001 } else { 0 };
    let id = format!("news-{n}@{list}");
    let mut text = headers(&Header {
        id: &id,
        from: (name, &format!("newsletter@{list}")),
        to: ME,
        subject: &subject,
        at,
        reply_to: None,
        status,
        list: Some(list),
    });
    text.push_str("Content-Type: text/html; charset=utf-8\n\n");
    text.push_str(&escape_body(&html));
    text.push('\n');
    text
}

/// Generates `count` messages spread over the 120 days before `now`.
fn generate(count: usize, seed: u64, now: i64) -> Vec<Generated> {
    let mut rng = Rng(seed);
    // Earlier messages per account: (id, subject, sender) for replies.
    let mut earlier: [Vec<Earlier>; 2] = [Vec::new(), Vec::new()];
    let span = 120 * 86_400;
    let mut out = Vec::with_capacity(count);
    for n in 0..count {
        // Oldest first, so replies follow their parents.
        let at = now - span
            + i64::try_from(n).unwrap_or(0) * span / i64::try_from(count.max(1)).unwrap_or(1)
            + i64::try_from(rng.below(3_600)).unwrap_or(0);
        let account = usize::from(rng.chance(55));
        if account == 0 && rng.chance(12) {
            out.push(Generated {
                folder: 3,
                text: newsletter(&mut rng, n, at),
            });
            continue;
        }
        let person = *rng.pick(PEOPLE);
        let reply = (!earlier[account].is_empty() && rng.chance(35))
            .then(|| earlier[account][rng.below(earlier[account].len())].clone());
        // Half the replies are ours; the rest come from someone else on the thread.
        let sent = if reply.is_some() {
            rng.chance(50)
        } else {
            rng.chance(10)
        };
        let (subject, parent, counterpart) = match &reply {
            Some((id, subject, who)) => {
                let base = subject.trim_start_matches("Re: ");
                // Someone else answers the original sender, unless we do.
                let other = if sent || person == *who { *who } else { person };
                (format!("Re: {base}"), Some(id.clone()), other)
            }
            None => (subject(&mut rng, account == 1, n), None, person),
        };
        let (from, to) = if sent {
            (ME, counterpart)
        } else {
            (counterpart, ME)
        };
        let id = format!("demo-{seed}-{n}@maze-demo.example");
        // Old mail is read and archived; recent mail sits in the inbox, partly unread.
        let age_days = (now - at) / 86_400;
        let folder_in_account = if sent {
            2
        } else if age_days > 30 && rng.chance(70) {
            1
        } else {
            0
        };
        let unread = !sent && folder_in_account == 0 && age_days < 21 && rng.chance(45);
        let flagged = rng.chance(7);
        let status = (if unread { 0 } else { 0x0001 }) | (if flagged { 0x0004 } else { 0 });
        let greeting = to.0.split_whitespace().next().unwrap_or("there");
        let mut body = paragraphs(&mut rng, greeting, from.0);
        if let Some((_, parent_subject, _)) = &reply {
            let _ = write!(
                body,
                "\nOn {}, {} wrote:\n> {}\n> {}\n",
                date(at - 86_400),
                to.0,
                rng.pick(SENTENCES),
                parent_subject
            );
        }
        let mut text = headers(&Header {
            id: &id,
            from,
            to,
            subject: &subject,
            at,
            reply_to: parent.as_deref(),
            status,
            list: None,
        });
        if rng.chance(14) {
            let (name, content_type) = *rng.pick(ATTACHMENTS);
            let mut data = format!("%PDF-1.4\n% Maze demo attachment {n}\n").into_bytes();
            data.extend((0..600 + rng.below(4_000)).map(|i| (i % 251) as u8));
            let boundary = format!("=_maze_demo_{n}");
            let _ = write!(
                text,
                "Content-Type: multipart/mixed; boundary=\"{boundary}\"\n\n--{boundary}\n\
                 Content-Type: text/plain; charset=utf-8\n\n{}\n--{boundary}\n\
                 Content-Type: {content_type}; name=\"{name}\"\n\
                 Content-Disposition: attachment; filename=\"{name}\"\n\
                 Content-Transfer-Encoding: base64\n\n{}\n--{boundary}--\n\n",
                escape_body(&body),
                base64(&data).replace("\r\n", "\n")
            );
        } else {
            text.push_str("Content-Type: text/plain; charset=utf-8\n\n");
            text.push_str(&escape_body(&body));
            text.push('\n');
        }
        if !sent {
            earlier[account].push((id, subject, from));
        }
        out.push(Generated {
            folder: FOLDERS
                .iter()
                .position(|(_, _, a)| *a == account)
                .unwrap_or(0)
                + folder_in_account,
            text,
        });
    }
    out
}

/// Fills the address book: addresses you wrote to (collected, most recent first) and an
/// imported Thunderbird book with vCards; the two overlap.
fn seed_contacts(w: &mut store::WriteTx<'_>, now: i64) -> store::Result<()> {
    for (ix, (name, email)) in PEOPLE.iter().enumerate().take(11) {
        let times = 1 + (ix % 4);
        for time in 0..times {
            let at = now - i64::try_from(ix * 2 + time).unwrap_or(0) * 86_400;
            w.bump_contact(email, Some(name), at)?;
        }
    }
    let phones = ["+43 660 1234567", "+49 30 5550142", "+1 415 555 0100"];
    for (ix, (name, email)) in PEOPLE.iter().enumerate().skip(6) {
        let company = COMPANIES[ix % COMPANIES.len()];
        let phone = phones[ix % phones.len()];
        let vcard = format!(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:{name}\r\nEMAIL:{email}\r\n\
             TEL;TYPE=cell:{phone}\r\nORG:{company}\r\nEND:VCARD\r\n"
        );
        w.upsert_contact("Personal Address Book", email, Some(name), Some(&vcard))?;
    }
    Ok(())
}

/// Writes `settings.json` sharing the demo accounts with agents, unless one exists.
fn write_settings(config_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(config_dir).with_context(|| format!("creating {}", config_dir.display()))?;
    let path = config_dir.join("settings.json");
    if path.exists() {
        eprintln!(
            "keeping the existing {} (agent access: set agent.accounts to \"any\" for the demo \
             accounts)",
            path.display()
        );
        return Ok(path);
    }
    let accounts: Vec<String> = ACCOUNTS
        .iter()
        .map(|name| format!("      \"{name}\": \"any\""))
        .collect();
    fs::write(
        &path,
        format!(
            "// Maze demo settings (cargo xtask seed-demo).\n{{\n  \"agent\": {{\n    \"accounts\": {{\n{}\n    }}\n  }}\n}}\n",
            accounts.join(",\n")
        ),
    )
    .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// What seeding stored.
#[derive(Debug, Clone, Copy, Default)]
struct Seeded {
    stored: u64,
    failed: u64,
}

/// Seeds the data directory. See the [module docs](self).
pub(crate) fn seed_demo(options: &SeedOptions) -> Result<()> {
    if options.messages == 0 {
        bail!("--messages must be at least 1");
    }
    fs::create_dir_all(&options.data_dir)
        .with_context(|| format!("creating {}", options.data_dir.display()))?;
    let settings = write_settings(&options.config_dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?;
    let key_store = Arc::new(vault::FileKeyStore::new(
        options.data_dir.join(DEV_KEY_FILE),
    ));
    let config = CoreConfig::new(&options.data_dir, key_store)
        .with_settings_file(&settings)
        .with_runtime(runtime.handle().clone());
    let core = runtime
        .block_on(CoreHandle::open(config))
        .context("opening the data directory (is Maze running on it?)")?;

    let now = OffsetDateTime::now_utc().unix_timestamp();
    let generated = generate(options.messages, options.seed, now);
    let mut files: Vec<String> = vec![String::new(); FOLDERS.len()];
    for message in generated {
        files[message.folder].push_str(&message.text);
    }

    let result = runtime.block_on(async {
        let ingestor = Ingestor::with_sink(core.store().clone(), Arc::new(core.index_sink()));
        let store = core.store().clone();
        let stats = tokio::task::spawn_blocking(move || -> Result<Seeded> {
            let accounts = store.write_blocking(|w| {
                ACCOUNTS
                    .iter()
                    .enumerate()
                    .map(|(ordinal, name)| {
                        w.create_account(&NewAccount {
                            kind: AccountKind::Imap,
                            name: (*name).to_owned(),
                            config_json: "{}".to_owned(),
                            ordinal: ordinal as i64,
                        })
                    })
                    .collect::<store::Result<Vec<_>>>()
            })?;
            store.write_blocking(move |w| seed_contacts(w, now))?;
            let mut total = Seeded::default();
            for (ix, (name, role, account)) in FOLDERS.iter().enumerate() {
                let account = accounts[*account];
                let (name, role) = ((*name).to_owned(), *role);
                let folder = store.write_blocking(move |w| {
                    w.create_folder(&NewFolder {
                        account,
                        parent: None,
                        remote_name: Some(if role == Some(FolderRole::Inbox) {
                            "INBOX".to_owned()
                        } else {
                            name.clone()
                        }),
                        display_name: name,
                        role,
                        is_label: false,
                        sync_policy: None,
                        ordinal: ix as i64,
                    })
                })?;
                let mut items = Vec::new();
                let reader = MboxReader::new(files[ix].as_bytes()).with_escaping(Escaping::Mboxrd);
                for (n, record) in reader.enumerate() {
                    let record = record?;
                    let r = import::reconstruct(&record.message);
                    let location = NewLocation {
                        folder,
                        uid: Some(u32::try_from(n + 1)?),
                        modseq: None,
                        flags: r.mozilla.flags(),
                        keywords: r.mozilla.keywords(),
                    };
                    items.push(RawItem {
                        meta: MessageMeta::new(account, location)
                            .with_internal_date(record.envelope.date),
                        raw: r.bytes,
                    });
                }
                for result in ingestor.ingest_raw_batch_blocking(items) {
                    match result {
                        Ok(_) => total.stored += 1,
                        Err(_) => total.failed += 1,
                    }
                }
            }
            Ok(total)
        })
        .await
        .context("the import thread failed")??;
        core.commit_index().await?;
        anyhow::Ok(stats)
    });
    // A tool run isn't an app launch: don't leave the crash guard's sentinel behind.
    if let Err(error) = core.mark_healthy() {
        eprintln!("warning: cannot clear the startup sentinel: {error}");
    }
    let shutdown = runtime.block_on(core.shutdown());
    let stats = result?;
    shutdown.context("closing the data directory")?;
    println!(
        "Seeded {} messages ({} failed) into {}",
        stats.stored,
        stats.failed,
        options.data_dir.display()
    );
    println!("\nUse it with:\n");
    println!("  export MAZEMAIL_DATA_DIR={}", options.data_dir.display());
    println!(
        "  export MAZEMAIL_CONFIG_DIR={}",
        options.config_dir.display()
    );
    println!("  export MAZEMAIL_INSECURE_DEV_KEYSTORE=1");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_is_deterministic_and_parses() {
        let now = 1_792_000_000;
        let a = generate(120, 7, now);
        let b = generate(120, 7, now);
        assert_eq!(
            a.iter().map(|m| &m.text).collect::<Vec<_>>(),
            b.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
        let mut replies = 0;
        let mut attachments = 0;
        for message in &a {
            assert!(message.text.starts_with("From "));
            let raw = message.text.split_once('\n').unwrap().1;
            let parsed = mazemail_mime::parse(raw.as_bytes()).unwrap();
            assert!(parsed.headers.subject.is_some());
            replies += usize::from(!parsed.headers.in_reply_to.is_empty());
            attachments += usize::from(parsed.has_attachments);
        }
        assert!(replies > 10, "{replies} replies");
        assert!(attachments > 3, "{attachments} attachments");
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b"Maze"), "TWF6ZQ==");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"Maz"), "TWF6");
    }

    #[test]
    fn seeding_imports_everything_and_is_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let options = SeedOptions {
            data_dir: dir.path().join("data"),
            config_dir: dir.path().join("config"),
            messages: 60,
            seed: 3,
        };
        seed_demo(&options).unwrap();
        assert!(options.data_dir.join(DEV_KEY_FILE).is_file());
        let settings = fs::read_to_string(options.config_dir.join("settings.json")).unwrap();
        assert!(settings.contains("\"Work\": \"any\""));

        // Reopen and look.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let key_store = Arc::new(vault::FileKeyStore::new(
            options.data_dir.join(DEV_KEY_FILE),
        ));
        let config =
            CoreConfig::new(&options.data_dir, key_store).with_runtime(runtime.handle().clone());
        let core = runtime.block_on(CoreHandle::open(config)).unwrap();
        let accounts: Vec<String> = core
            .accounts()
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(accounts, ["Personal", "Work"]);
        let found = runtime
            .block_on(core.search("best", mazemail_core::SearchOpts::default()))
            .unwrap();
        assert!(found.total_messages > 10, "{}", found.total_messages);
        // The address book: collected and imported, some people in both.
        let books: Vec<String> = core
            .address_books()
            .unwrap()
            .into_iter()
            .map(|b| b.name)
            .collect();
        assert_eq!(books, ["Collected", "Personal Address Book"]);
        let contacts = core
            .contacts(&mazemail_core::ContactQuery::default())
            .unwrap();
        assert_eq!(contacts.len(), PEOPLE.len());
        assert!(contacts.iter().any(|c| c.labels.len() == 2));
        runtime.block_on(core.shutdown()).unwrap();
    }
}

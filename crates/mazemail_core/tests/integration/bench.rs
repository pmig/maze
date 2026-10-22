//! Benchmark at 100k messages (CONCEPT §3): folder switch, thread list, Quick Filter
//! and search p95, with encryption on.
//!
//! Ignored by default; run in release:
//!
//! ```sh
//! cargo nextest run -p mazemail_core --release --run-ignored only --no-capture bench
//! ```

use std::time::{Duration, Instant};

use mazemail_core::{IndexDocument, ListQuery, ListScope, QuickFilter, SearchOpts};
use mazemail_types::{AccountId, Address, Flags, FolderId, Sensitive};
use store::{NewLocation, NewMessage, ThreadAssignment};

use crate::common;

const INBOX: u32 = 100_000;
const ARCHIVE: u32 = 20_000;
const JOB: u32 = 5_000;

const WORDS: [&str; 32] = [
    "invoice",
    "meeting",
    "budget",
    "review",
    "quarterly",
    "report",
    "project",
    "deadline",
    "travel",
    "lunch",
    "contract",
    "draft",
    "release",
    "update",
    "customer",
    "support",
    "design",
    "launch",
    "schedule",
    "agenda",
    "proposal",
    "feedback",
    "summary",
    "notes",
    "server",
    "backup",
    "holiday",
    "office",
    "hiring",
    "training",
    "security",
    "payment",
];

fn words(seed: u32, count: usize) -> String {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..count)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            WORDS[(x as usize) % WORDS.len()]
        })
        .collect::<Vec<_>>()
        .join(" ")
}

async fn seed(
    core: &mazemail_core::CoreHandle,
    account: AccountId,
    folder: FolderId,
    base: u32,
    count: u32,
) {
    let sink = core.index_sink();
    let mut next = 0;
    while next < count {
        let end = (next + JOB).min(count);
        let range = (base + next)..(base + end);
        let inserted = core
            .store()
            .write_weighted(JOB as usize, move |w| {
                let mut out = Vec::new();
                for i in range {
                    let mut m = NewMessage::new(account, 1_600_000_000 + i64::from(i) * 60);
                    m.rfc_message_id = Some(format!("{i}.bench@example.com"));
                    m.subject = Some(format!("{} {i}", words(i, 4)));
                    m.from = Some(Address::new(
                        Some(format!("Sender {}", i % 2000)),
                        format!("sender{}@example.com", i % 2000),
                    ));
                    m.to = vec![Address::new(None::<String>, "me@work.example")];
                    m.snippet = Some(words(i + 7, 12));
                    m.size = Some(4096);
                    m.has_attachments = i.is_multiple_of(11);
                    m.thread = ThreadAssignment::New;
                    let flags = if i.is_multiple_of(4) {
                        Flags::empty()
                    } else {
                        Flags::SEEN
                    };
                    m.locations = vec![NewLocation::new(folder, Some(i + 1), flags)];
                    let outcome = w.upsert_message(&m)?;
                    out.push((outcome, m));
                }
                Ok(out)
            })
            .await
            .unwrap();
        for (outcome, m) in inserted {
            let i = outcome.message.0 as u32;
            sink.add(IndexDocument {
                message_id: outcome.message,
                thread_id: outcome.thread,
                account_id: account,
                date: m.date,
                size: 4096,
                from: m.from.into_iter().collect(),
                to: m.to,
                cc: Vec::new(),
                subject: Sensitive::new(m.subject.unwrap_or_default()),
                body_text: Sensitive::new(words(i + 13, 120)),
                attachment_names: Vec::new(),
                list_id: None,
                has_attachment: m.has_attachments,
                language: None,
            })
            .unwrap();
        }
        next = end;
    }
}

fn stats(label: &str, mut times: Vec<Duration>) -> (Duration, Duration) {
    times.sort();
    let median = times[times.len() / 2];
    let p95 = times[(times.len() * 95 / 100).min(times.len() - 1)];
    eprintln!("{label:<40} median {median:>10.2?}  p95 {p95:>10.2?}");
    (median, p95)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark: 120k messages, run in release"]
async fn bench_100k() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let started = Instant::now();
    seed(&t.core, f.work, f.inbox, 0, INBOX).await;
    seed(&t.core, f.work, f.archive, INBOX, ARCHIVE).await;
    t.core.commit_index().await.unwrap();
    eprintln!(
        "seeded and indexed {} messages in {:.1?}",
        INBOX + ARCHIVE,
        started.elapsed()
    );

    let inbox = ListQuery::new(ListScope::Folder(f.inbox));
    let archive = ListQuery::new(ListScope::Folder(f.archive));

    // Folder switch: keys of the whole folder plus the first screen of rows.
    let mut switch = Vec::new();
    for run in 0..20 {
        let query = if run % 2 == 0 { &inbox } else { &archive };
        let started = Instant::now();
        let snapshot = t.core.thread_list(query).unwrap();
        let rows = t
            .core
            .rows(&snapshot.thread_ids(0..60), snapshot.context())
            .unwrap();
        switch.push(started.elapsed());
        assert_eq!(rows.len(), 60);
    }
    let (_, switch_p95) = stats("folder switch (100k / 20k)", switch);

    let mut keys = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let snapshot = t.core.thread_list(&inbox).unwrap();
        keys.push(started.elapsed());
        assert_eq!(snapshot.len(), INBOX as usize);
    }
    stats("thread list keys (100k)", keys);

    let unread = ListQuery {
        quick_filter: QuickFilter {
            unread: true,
            ..QuickFilter::default()
        },
        ..inbox.clone()
    };
    let mut toggle = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        let snapshot = t.core.thread_list(&unread).unwrap();
        toggle.push(started.elapsed());
        assert_eq!(snapshot.len(), INBOX as usize / 4);
    }
    let (toggle_median, _) = stats("quick filter: unread (100k)", toggle);

    let text = |s: &str| ListQuery {
        quick_filter: QuickFilter {
            text: Some(Sensitive::new(s.to_owned())),
            ..QuickFilter::default()
        },
        ..inbox.clone()
    };
    let started = Instant::now();
    t.core.thread_list(&text("s")).unwrap();
    eprintln!(
        "{:<40} {:>10.2?}",
        "quick filter: first keystroke (builds text)",
        started.elapsed()
    );
    let mut typed = Vec::new();
    for word in ["se", "sen", "sender 4", "sender 42", "invoice"] {
        let started = Instant::now();
        t.core.thread_list(&text(word)).unwrap();
        typed.push(started.elapsed());
    }
    stats("quick filter: next keystrokes (memory)", typed);
    let mut body = Vec::new();
    for word in ["budget", "holiday payment", "zzz"] {
        let started = Instant::now();
        t.core.quick_filter_body(&text(word)).await.unwrap();
        body.push(started.elapsed());
    }
    stats("quick filter: body matches (index)", body);

    let queries = [
        "invoice",
        "budget review",
        "from:sender42",
        "\"quarterly report\"",
        "invoice is:unread",
        "meeting in:inbox",
        "subject:launch has:attachment",
        "payment -in:archive",
        "security OR holiday",
        "deadline after:2020-12-01",
    ];
    let mut search = Vec::new();
    for _ in 0..5 {
        for q in queries {
            let started = Instant::now();
            let results = t.core.search(q, SearchOpts::default()).await.unwrap();
            search.push(started.elapsed());
            assert!(results.total_messages > 0, "{q}");
        }
    }
    let (_, search_p95) = stats("search (snippets, 50 threads)", search);
    let mut faceted = Vec::new();
    for q in ["invoice", "meeting in:inbox"] {
        let started = Instant::now();
        t.core
            .search(
                q,
                SearchOpts {
                    facets: true,
                    ..SearchOpts::default()
                },
            )
            .await
            .unwrap();
        faceted.push(started.elapsed());
    }
    stats("search with facets", faceted);

    if !cfg!(debug_assertions) {
        assert!(
            switch_p95 < Duration::from_millis(50),
            "folder switch {switch_p95:?}"
        );
        assert!(
            toggle_median < Duration::from_millis(16),
            "unread toggle {toggle_median:?}"
        );
        assert!(
            search_p95 < Duration::from_millis(100),
            "search p95 {search_p95:?}"
        );
    }
    t.core.shutdown().await.unwrap();
}

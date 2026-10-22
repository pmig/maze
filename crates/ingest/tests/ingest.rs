//! Integration tests: the ingest pipeline against a real (encrypted) store.

// Tests may unwrap (AGENTS.md); clippy only knows that for `#[test]` functions, not for
// the helpers they share.
#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use ingest::{
    BodySummary, GmailMeta, IndexLanguage, Ingestor, MemoryIndexSink, MessageMeta, RawItem,
};
use mazemail_types::{AccountId, Flags, FolderId, FolderRole, MessageId};
use pretty_assertions::assert_eq;
use store::{
    AccountKind, BodyState, NewAccount, NewFolder, NewLocation, Queries, Store, StoreConfig,
};
use vault::{KEY_LEN, KeyId, MasterKey, Vault};

fn open(dir: &Path) -> Store {
    let mut config = StoreConfig::new(dir);
    config.gc_interval = None;
    let vault = Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [7; KEY_LEN]));
    Store::open(&config, &vault).unwrap()
}

fn close(store: Store) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(store.close())
        .unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    sink: Arc<MemoryIndexSink>,
    ingestor: Ingestor,
    account: AccountId,
    inbox: FolderId,
    archive: FolderId,
}

fn fixture(kind: AccountKind) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let (account, inbox, archive) = store
        .write_blocking(move |w| {
            let account = w.create_account(&NewAccount {
                kind,
                name: "Work".to_owned(),
                config_json: "{}".to_owned(),
                ordinal: 0,
            })?;
            let inbox = w.create_folder(
                &NewFolder::new(account, "INBOX", "Inbox").with_role(FolderRole::Inbox),
            )?;
            let archive = w.create_folder(&NewFolder::new(account, "Archive", "Archive"))?;
            Ok((account, inbox, archive))
        })
        .unwrap();
    let sink = Arc::new(MemoryIndexSink::new());
    let ingestor = Ingestor::with_sink(store.clone(), sink.clone());
    Fixture {
        _dir: dir,
        store,
        sink,
        ingestor,
        account,
        inbox,
        archive,
    }
}

fn meta(f: &Fixture, folder: FolderId, uid: u32) -> MessageMeta {
    MessageMeta::new(
        f.account,
        NewLocation::new(folder, Some(uid), Flags::empty()),
    )
}

fn mail(id: &str, subject: &str, extra_headers: &str, body: &str) -> Vec<u8> {
    format!(
        "Date: Tue, 1 Oct 2024 10:00:00 +0200\r\n\
         From: Anna Example <anna@example.com>\r\n\
         To: Ben <ben@example.org>\r\n\
         Cc: carla@example.net\r\n\
         Subject: {subject}\r\n\
         Message-ID: <{id}>\r\n\
         {extra_headers}\
         \r\n\
         {body}\r\n"
    )
    .into_bytes()
}

const ATTACHMENT_MAIL: &str = "Date: Wed, 2 Oct 2024 09:30:00 +0000\r\n\
From: Dora <dora@example.com>\r\n\
To: ben@example.org\r\n\
Subject: Rechnung Oktober\r\n\
Message-ID: <invoice@example.com>\r\n\
List-Id: Billing <billing.example.com>\r\n\
Delivered-To: ben@example.org\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hallo Ben, anbei die Rechnung für den Monat Oktober. Bitte überweise den Betrag \
bis zum Ende der Woche auf das bekannte Konto. Vielen Dank und viele Grüße.\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"rechnung.pdf\"\r\n\
Content-Disposition: attachment; filename=\"rechnung.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--b1--\r\n";

#[test]
fn ingest_raw_stores_summary_body_and_index_doc() {
    let f = fixture(AccountKind::Imap);
    let raw = ATTACHMENT_MAIL.as_bytes();
    let done = f
        .ingestor
        .ingest_raw_blocking(
            meta(&f, f.inbox, 1).with_internal_date(Some(1_727_900_000)),
            raw,
        )
        .unwrap();
    assert!(done.created);

    let detail = f
        .store
        .reader()
        .unwrap()
        .message(done.message)
        .unwrap()
        .unwrap();
    assert_eq!(
        detail.rfc_message_id.as_deref(),
        Some("invoice@example.com")
    );
    assert_eq!(detail.subject.as_deref(), Some("Rechnung Oktober"));
    assert_eq!(detail.list_id.as_deref(), Some("billing.example.com"));
    assert_eq!(detail.delivered_to.as_deref(), Some("ben@example.org"));
    assert_eq!(detail.size, Some(raw.len() as u64));
    assert_eq!(detail.received, Some(1_727_900_000));
    assert_eq!(detail.date, 1_727_861_400);
    assert!(detail.has_attachments);
    assert!(detail.snippet.as_deref().unwrap().starts_with("Hallo Ben"));
    assert!(
        detail
            .bodystructure_json
            .as_deref()
            .unwrap()
            .contains("application/pdf")
    );
    assert_eq!(detail.body_state, BodyState::Full);
    let blob = f.store.blobs().read(&detail.blob.unwrap()).unwrap();
    assert_eq!(blob, raw);

    let doc = f.sink.get(done.message).unwrap();
    assert_eq!(doc, done.doc);
    assert_eq!(doc.thread, done.thread);
    assert_eq!(doc.account, f.account);
    assert_eq!(doc.attachment_names, vec!["rechnung.pdf".to_owned()]);
    assert_eq!(doc.list_id.as_deref(), Some("billing.example.com"));
    assert_eq!(doc.language, Some(IndexLanguage::De));
    assert!(doc.has_attachment);
    assert!(doc.body.contains("überweise"));
    assert_eq!(doc.from[0].address, "dora@example.com");
    assert_eq!(doc.to[0].address, "ben@example.org");
    close(f.store);
}

#[test]
fn same_uid_updates_instead_of_duplicating() {
    let f = fixture(AccountKind::Imap);
    let raw = mail("a@example.com", "Hello", "", "Hi");
    let first = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 5), &raw)
        .unwrap();
    let mut again = meta(&f, f.inbox, 5);
    again.locations[0].flags = Flags::SEEN;
    let second = f.ingestor.ingest_raw_blocking(again, &raw).unwrap();
    assert!(first.created);
    assert!(!second.created);
    assert_eq!(first.message, second.message);
    let detail = f
        .store
        .reader()
        .unwrap()
        .message(first.message)
        .unwrap()
        .unwrap();
    assert_eq!(detail.effective_flags(), Flags::SEEN);
    close(f.store);
}

#[test]
fn replies_join_and_late_parents_merge() {
    let f = fixture(AccountKind::Imap);
    let root = mail("root@x", "Plan", "", "root");
    let reply = mail(
        "reply@x",
        "Re: Plan",
        "In-Reply-To: <root@x>\r\nReferences: <root@x>\r\n",
        "reply",
    );
    // A second conversation that turns out to belong to the first one.
    let orphan = mail(
        "orphan@x",
        "Re: Plan",
        "References: <missing@x>\r\n",
        "orphan",
    );
    let bridge = mail(
        "missing@x",
        "Re: Plan",
        "References: <root@x>\r\n",
        "bridge",
    );
    let unrelated = mail("other@x", "Lunch", "", "lunch");

    let r = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 1), &root)
        .unwrap();
    let a = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 2), &reply)
        .unwrap();
    let o = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.archive, 3), &orphan)
        .unwrap();
    let u = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 4), &unrelated)
        .unwrap();
    assert_eq!(r.thread, a.thread);
    assert_ne!(o.thread, r.thread);
    assert_ne!(u.thread, r.thread);

    let b = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 5), &bridge)
        .unwrap();
    let reader = f.store.reader().unwrap();
    let thread_of = |id: MessageId| reader.message(id).unwrap().unwrap().thread;
    assert_eq!(thread_of(o.message), b.thread);
    assert_eq!(thread_of(r.message), b.thread);
    assert_eq!(reader.thread_message_ids(b.thread).unwrap().len(), 4);
    assert_ne!(thread_of(u.message), b.thread);
    drop(reader);
    close(f.store);
}

#[test]
fn replies_without_references_join_by_subject() {
    let f = fixture(AccountKind::Imap);
    let root = mail("s1@x", "Budget 2025", "", "root");
    let reply = mail("s2@x", "AW: Budget 2025", "", "reply without references");
    let not_reply = mail("s3@x", "Budget 2025", "", "same subject, no prefix");
    let r = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 1), &root)
        .unwrap();
    let a = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 2), &reply)
        .unwrap();
    let n = f
        .ingestor
        .ingest_raw_blocking(meta(&f, f.inbox, 3), &not_reply)
        .unwrap();
    assert_eq!(r.thread, a.thread);
    assert_ne!(r.thread, n.thread);
    close(f.store);
}

#[test]
fn gmail_thread_ids_decide() {
    let f = fixture(AccountKind::Gmail);
    let gmail = |uid: u32, msgid: u64, thrid: u64| {
        let mut m = meta(&f, f.inbox, uid);
        m.gmail = Some(GmailMeta {
            msgid,
            thrid,
            replace_labels: false,
        });
        m
    };
    let a = mail("g1@x", "One", "", "a");
    // References the first message but Gmail says it's another thread.
    let b = mail("g2@x", "Re: One", "References: <g1@x>\r\n", "b");
    let c = mail("g3@x", "Unrelated", "", "c");
    let ra = f
        .ingestor
        .ingest_raw_blocking(gmail(1, 100, 900), &a)
        .unwrap();
    let rb = f
        .ingestor
        .ingest_raw_blocking(gmail(2, 101, 901), &b)
        .unwrap();
    let rc = f
        .ingestor
        .ingest_raw_blocking(gmail(3, 102, 900), &c)
        .unwrap();
    assert_ne!(ra.thread, rb.thread);
    assert_eq!(ra.thread, rc.thread);
    // The same X-GM-MSGID in another folder is the same message.
    let again = f.ingestor.ingest_raw_blocking(
        {
            let mut m = gmail(7, 100, 900);
            m.locations[0].folder = f.archive;
            m
        },
        &a,
    );
    let again = again.unwrap();
    assert_eq!(again.message, ra.message);
    assert!(!again.created);
    close(f.store);
}

#[test]
fn batches_see_their_own_messages_and_isolate_failures() {
    let f = fixture(AccountKind::Imap);
    let other = fixture(AccountKind::Imap);
    let items = vec![
        RawItem {
            meta: meta(&f, f.inbox, 1),
            raw: mail("b1@x", "Batch", "", "one"),
        },
        RawItem {
            meta: meta(&f, f.inbox, 2),
            raw: mail("b2@x", "Re: Batch", "References: <b1@x>\r\n", "two"),
        },
        // A folder of another store's account: the store rejects it.
        RawItem {
            meta: MessageMeta::new(
                f.account,
                NewLocation::new(FolderId(other.inbox.0 + 1000), Some(3), Flags::empty()),
            ),
            raw: mail("b3@x", "Broken", "", "three"),
        },
        RawItem {
            meta: meta(&f, f.inbox, 4),
            raw: Vec::new(),
        },
        RawItem {
            meta: meta(&f, f.inbox, 5),
            raw: mail("b5@x", "Fine", "", "five"),
        },
    ];
    let results = f.ingestor.ingest_raw_batch_blocking(items);
    assert_eq!(results.len(), 5);
    let one = results[0].as_ref().unwrap();
    let two = results[1].as_ref().unwrap();
    assert_eq!(one.thread, two.thread);
    assert!(matches!(results[2], Err(ingest::Error::Store(_))));
    assert!(matches!(results[3], Err(ingest::Error::Parse(_))));
    assert!(results[4].is_ok());
    assert_eq!(f.sink.len(), 3);
    close(f.store);
    close(other.store);
}

#[test]
fn headers_first_then_body() {
    let f = fixture(AccountKind::Imap);
    let raw = ATTACHMENT_MAIL.as_bytes();
    let header_end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let summary = BodySummary {
        size: Some(raw.len() as u64),
        has_attachments: true,
        attachment_names: vec!["rechnung.pdf".to_owned()],
        bodystructure_json: None,
        snippet: None,
    };
    let headers = f
        .ingestor
        .ingest_headers_blocking(meta(&f, f.inbox, 9), &raw[..header_end], summary)
        .unwrap();
    let reader = f.store.reader().unwrap();
    let detail = reader.message(headers.message).unwrap().unwrap();
    assert_eq!(detail.body_state, BodyState::None);
    assert!(detail.blob.is_none());
    assert!(detail.snippet.is_none());
    assert!(detail.has_attachments);
    assert_eq!(f.sink.get(headers.message).unwrap().body, "");

    let full = f
        .ingestor
        .attach_body_blocking(headers.message, raw)
        .unwrap();
    assert_eq!(full.message, headers.message);
    let detail = reader.message(headers.message).unwrap().unwrap();
    assert_eq!(detail.body_state, BodyState::Full);
    assert_eq!(f.store.blobs().read(&detail.blob.unwrap()).unwrap(), raw);
    assert!(detail.snippet.as_deref().unwrap().starts_with("Hallo Ben"));
    assert!(detail.bodystructure_json.is_some());
    assert_eq!(detail.locations.len(), 1);
    let doc = f.sink.get(headers.message).unwrap();
    assert!(doc.body.contains("Rechnung"));
    assert_eq!(doc.attachment_names, vec!["rechnung.pdf".to_owned()]);
    assert_eq!(f.sink.len(), 1);
    drop(reader);

    assert!(
        f.ingestor
            .attach_body_blocking(MessageId(9999), raw)
            .is_err()
    );
    close(f.store);
}

#[test]
fn rejects_messages_without_locations() {
    let f = fixture(AccountKind::Imap);
    let mut m = meta(&f, f.inbox, 1);
    m.locations.clear();
    let err = f
        .ingestor
        .ingest_raw_blocking(m, &mail("x@x", "s", "", "b"))
        .unwrap_err();
    assert!(matches!(err, ingest::Error::Invalid(_)));
    close(f.store);
}

#[test]
fn async_api_works_on_a_runtime() {
    let f = fixture(AccountKind::Imap);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let ingestor = f.ingestor.clone();
    let (inbox, account) = (f.inbox, f.account);
    let count = runtime.block_on(async move {
        let one = ingestor
            .ingest_raw(
                MessageMeta::new(account, NewLocation::new(inbox, Some(1), Flags::empty())),
                &mail("async@x", "Async", "", "body"),
            )
            .await
            .unwrap();
        let header = ingestor
            .ingest_headers(
                MessageMeta::new(account, NewLocation::new(inbox, Some(2), Flags::empty())),
                b"Message-ID: <h@x>\r\nSubject: Re: Async\r\nReferences: <async@x>\r\n\r\n",
                BodySummary::default(),
            )
            .await
            .unwrap();
        assert_eq!(one.thread, header.thread);
        ingestor
            .attach_body(
                header.message,
                b"Message-ID: <h@x>\r\nSubject: Re: Async\r\nReferences: <async@x>\r\n\r\nbody\r\n",
            )
            .await
            .unwrap();
        let batch = ingestor
            .ingest_raw_batch(vec![RawItem {
                meta: MessageMeta::new(account, NewLocation::new(inbox, Some(3), Flags::empty())),
                raw: mail("async2@x", "x", "", "y"),
            }])
            .await;
        batch.len()
    });
    assert_eq!(count, 1);
    assert_eq!(f.sink.len(), 3);
    drop(runtime);
    close(f.store);
}

/// Ingest throughput by batch size. Run with
/// `cargo nextest run -p ingest --release --run-ignored only perf --no-capture`.
#[test]
#[ignore = "performance measurement"]
fn perf() {
    const N: u32 = 4000;
    for batch in [1usize, 50, 200, 500] {
        let f = fixture(AccountKind::Imap);
        let messages: Vec<Vec<u8>> = (0..N)
            .map(|i| {
                let refs = if i % 4 == 0 {
                    String::new()
                } else {
                    format!("References: <perf-{}@x>\r\n", i - i % 4)
                };
                mail(
                    &format!("perf-{i}@x"),
                    &format!("Subject {}", i / 4),
                    &refs,
                    &"Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(40),
                )
            })
            .collect();
        let started = Instant::now();
        let mut prepare = std::time::Duration::ZERO;
        for (chunk_index, chunk) in messages.chunks(batch).enumerate() {
            let t = Instant::now();
            let prepared: Vec<_> = chunk
                .iter()
                .enumerate()
                .map(|(i, raw)| {
                    let uid = (chunk_index * batch + i + 1) as u32;
                    f.ingestor.prepare_raw(meta(&f, f.inbox, uid), raw).unwrap()
                })
                .collect();
            prepare += t.elapsed();
            for result in f.ingestor.commit_blocking(prepared) {
                result.unwrap();
            }
        }
        let total = started.elapsed();
        println!(
            "batch {batch:>4}: {N} messages in {total:.2?} ({:.0} msg/s), prepare (parse + blob) {prepare:.2?}, commit {:.2?}",
            f64::from(N) / total.as_secs_f64(),
            total - prepare
        );
        close(f.store);
    }

    // Where prepare time goes: parsing alone, and blob writes alone (sequential and
    // from 8 threads; each put fsyncs).
    let raw = mail("p@x", "s", "", &"Lorem ipsum dolor sit amet. ".repeat(80));
    let t = Instant::now();
    for _ in 0..1000 {
        mazemail_mime::parse(&raw).unwrap();
    }
    println!("parse: {:.1} µs/message", t.elapsed().as_secs_f64() * 1e3);
    let f = fixture(AccountKind::Imap);
    let t = Instant::now();
    for i in 0..200u32 {
        let mut unique = raw.clone();
        unique.extend_from_slice(&i.to_le_bytes());
        f.store.blobs().put(&unique).unwrap();
    }
    println!(
        "blob put, 1 thread: {:.2} ms/blob",
        t.elapsed().as_secs_f64() * 1e3 / 200.0
    );
    let t = Instant::now();
    std::thread::scope(|scope| {
        for thread in 0..8u32 {
            let blobs = f.store.blobs().clone();
            let raw = raw.clone();
            scope.spawn(move || {
                for i in 0..100u32 {
                    let mut unique = raw.clone();
                    unique.extend_from_slice(&(thread * 1000 + i + 10_000).to_le_bytes());
                    blobs.put(&unique).unwrap();
                }
            });
        }
    });
    println!(
        "blob put, 8 threads: {:.0} blobs/s",
        800.0 / t.elapsed().as_secs_f64()
    );
    close(f.store);
}

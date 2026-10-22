//! End-to-end: a real index on an `EncryptedDirectory`.

#![allow(clippy::unwrap_used)] // test helpers

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mazemail_types::{AccountId, Address, MessageId, Sensitive, ThreadId};
use search::query::parse;
use search::{
    Attachment, BlockCache, DirectoryOptions, IndexDoc, IndexOptions, Language, PlanContext,
    QueryPlan, SearchIndex, SearchOptions, TantivyPart,
};
use vault::{BlockCipher, KEY_LEN, KeyId, MasterKey};

fn cipher(seed: u8) -> BlockCipher {
    BlockCipher::new(&MasterKey::from_bytes(KeyId::FIRST, [seed; KEY_LEN]))
}

fn options(block_size: u32) -> DirectoryOptions {
    DirectoryOptions {
        block_size,
        cache: Arc::new(BlockCache::new(4 * 1024 * 1024)),
    }
}

/// Words that appear nowhere else, so finding them on disk means plaintext leaked.
const SECRETS: [&str; 6] = [
    "zyxquorbit",
    "plumbagovex",
    "Quaggamoth",
    "snorkelwyrm",
    "vexillarium",
    "grommetzauber",
];

fn corpus() -> Vec<IndexDoc> {
    (0..200)
        .map(|i: i64| {
            let secret = SECRETS[i as usize % SECRETS.len()];
            let filler = "Lorem ipsum dolor sit amet. ".repeat(i as usize % 20);
            IndexDoc {
                message_id: MessageId(i),
                thread_id: ThreadId(i / 3),
                account_id: AccountId(1),
                date: 1_767_225_600 + i * 3600,
                size: 4096 + i as u64,
                from: vec![Address::new(
                    Some("Hieronymus Bosch"),
                    format!("{secret}{i}@painters.example"),
                )],
                to: vec![Address::new(None::<String>, "garden@delights.example")],
                cc: Vec::new(),
                subject: Sensitive::new(format!("Subject {secret} number {i}")),
                body: Sensitive::new(if i % 2 == 0 {
                    format!("Die Rechnungen für {secret} liegen bei. {filler}")
                } else {
                    format!("Invoices for {secret} attached. {filler}")
                }),
                attachments: vec![Attachment {
                    name: Sensitive::new(format!("{secret}-report.pdf")),
                    text: Some(Sensitive::new(format!("attachment text {secret}"))),
                }],
                list_id: Some("triptych.lists.example".to_owned()),
                language: Some(if i % 2 == 0 {
                    Language::De
                } else {
                    Language::En
                }),
            }
        })
        .collect()
}

fn fill(index: &SearchIndex) {
    let mut indexer = index.indexer(&IndexOptions::default()).unwrap();
    for doc in corpus() {
        indexer.add(&doc).unwrap();
    }
    indexer.finish().unwrap();
}

fn count(index: &SearchIndex, query: &str) -> usize {
    let context = PlanContext::now_utc();
    let plan = QueryPlan::new(&parse(query).unwrap().query, &context).unwrap();
    let part = plan.text.unwrap_or_else(TantivyPart::everything);
    index
        .searcher()
        .search(
            &part,
            &SearchOptions {
                limit: 10_000,
                ..SearchOptions::default()
            },
        )
        .unwrap()
        .len()
}

fn files(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .collect()
}

/// Every occurrence of any secret (as written and lowercased, the way terms are stored).
fn plaintext_hits(root: &Path) -> Vec<(PathBuf, String)> {
    let mut hits = Vec::new();
    for path in files(root) {
        let bytes = std::fs::read(&path).unwrap();
        for secret in SECRETS {
            for needle in [secret.to_owned(), secret.to_lowercase()] {
                if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                    hits.push((path.clone(), needle));
                }
            }
        }
        for needle in [
            "Hieronymus",
            "hieronymus",
            "painters",
            "delights",
            "Rechnungen",
        ] {
            if bytes.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                hits.push((path.clone(), needle.to_owned()));
            }
        }
    }
    hits
}

#[test]
fn encrypted_index_searches_and_reopens() {
    let temp = tempfile::tempdir().unwrap();
    let index = SearchIndex::open_encrypted(temp.path(), cipher(1), options(16 * 1024)).unwrap();
    fill(&index);
    assert_eq!(index.num_docs(), 200);
    assert_eq!(count(&index, "zyxquorbit"), 34);
    assert_eq!(count(&index, "from:painters.example"), 200);
    assert_eq!(count(&index, "Rechnung"), 100, "German stemmed copies");
    assert_eq!(count(&index, "invoice"), 100, "English stemmed copies");
    assert_eq!(count(&index, "filename:pdf has:attachment"), 200);
    drop(index);

    let reopened = SearchIndex::open_encrypted(temp.path(), cipher(1), options(16 * 1024)).unwrap();
    assert_eq!(reopened.num_docs(), 200);
    assert_eq!(count(&reopened, "\"subject plumbagovex\""), 34);
}

#[test]
fn no_plaintext_on_disk() {
    for block_size in [4 * 1024, 16 * 1024, 64 * 1024] {
        let temp = tempfile::tempdir().unwrap();
        let index =
            SearchIndex::open_encrypted(temp.path(), cipher(1), options(block_size)).unwrap();
        fill(&index);
        assert!(!files(temp.path()).is_empty());
        let hits = plaintext_hits(temp.path());
        assert!(
            hits.is_empty(),
            "block size {block_size}: plaintext found: {hits:?}"
        );
    }

    // Control: the same corpus on a plain MmapDirectory does contain the words, so the
    // scan above would have found them.
    let temp = tempfile::tempdir().unwrap();
    let plain =
        SearchIndex::open(tantivy::directory::MmapDirectory::open(temp.path()).unwrap()).unwrap();
    fill(&plain);
    assert!(!plaintext_hits(temp.path()).is_empty());
}

#[test]
fn wrong_key_cannot_open() {
    let temp = tempfile::tempdir().unwrap();
    let index = SearchIndex::open_encrypted(temp.path(), cipher(1), options(16 * 1024)).unwrap();
    fill(&index);
    drop(index);
    assert!(SearchIndex::open_encrypted(temp.path(), cipher(2), options(16 * 1024)).is_err());
}

#[test]
fn tampered_segment_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let index = SearchIndex::open_encrypted(temp.path(), cipher(1), options(4 * 1024)).unwrap();
    fill(&index);
    drop(index);
    // Flip one byte in the middle of every segment file.
    for path in files(temp.path()) {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".json") || name.ends_with(".lock") {
            continue;
        }
        let mut bytes = std::fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        std::fs::write(&path, bytes).unwrap();
    }
    let result =
        SearchIndex::open_encrypted(temp.path(), cipher(1), options(4 * 1024)).and_then(|index| {
            // Opening may succeed lazily; reading every posting must not.
            let searcher = index.searcher();
            searcher.count(&TantivyPart::text("zyxquorbit"), &Default::default())?;
            searcher.facets(&TantivyPart::everything(), &Default::default())?;
            Ok(())
        });
    assert!(result.is_err(), "tampering must be detected");
}

//! The offline body reuse index on the fake profile's `ImapMail` stores.

use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

use import::mbox::MboxReader;
use import::{BodyReuse, NoProgress, reconstruct};
use ingest::{BodyLookup, HeaderFingerprint};
use pretty_assertions::assert_eq;

use crate::common::{Env, thunderbird_copy, vault};

/// `(Message-ID, reconstructed bytes)` of every message in an mbox file.
fn messages(path: &Path) -> Vec<(Option<String>, Vec<u8>)> {
    MboxReader::new(BufReader::new(File::open(path).unwrap()))
        .map(|r| {
            let bytes = reconstruct(&r.unwrap().message).bytes;
            let id = mazemail_mime::parse_headers_only(&bytes)
                .unwrap()
                .message_id;
            (id, bytes)
        })
        .collect()
}

fn key(bytes: &[u8]) -> (u64, HeaderFingerprint) {
    (
        bytes.len() as u64,
        HeaderFingerprint::from_raw(bytes).unwrap(),
    )
}

#[test]
fn matches_only_unambiguous_exact_entries() {
    let env = Env::new();
    let (_, profile) = thunderbird_copy(&env);
    let imap = profile.join("ImapMail");
    let index_path = env.scratch("data").join("reuse.db");
    let reuse = BodyReuse::create(&index_path, &vault(3)).unwrap();
    let stats = reuse
        .scan(std::slice::from_ref(&imap), &NoProgress)
        .unwrap();
    assert_eq!(stats.mbox_files, 4);
    assert_eq!(stats.messages, 11);
    assert_eq!(stats.indexed, 10);
    assert_eq!(stats.without_message_id, 1);
    assert_eq!(reuse.len().unwrap(), 10);
    assert!(stats.bytes > 0);
    // Scanning again replaces the entries instead of adding duplicates.
    reuse
        .scan(std::slice::from_ref(&imap), &NoProgress)
        .unwrap();
    assert_eq!(reuse.len().unwrap(), 10);

    let inbox = messages(&imap.join("imap.example.com").join("INBOX"));
    let by_id = |id: &str| -> Vec<Vec<u8>> {
        inbox
            .iter()
            .filter(|(i, _)| i.as_deref() == Some(id))
            .map(|(_, b)| b.clone())
            .collect()
    };

    // A unique message comes back byte for byte (and with angle brackets too).
    let a1 = &by_id("a1@example.com")[0];
    let (size, fp) = key(a1);
    assert_eq!(reuse.lookup("a1@example.com", size, &fp).as_ref(), Some(a1));
    assert_eq!(
        reuse.lookup("<a1@example.com>", size, &fp).as_ref(),
        Some(a1)
    );
    // The stored copy had escaped `From ` lines and Thunderbird headers.
    let text = String::from_utf8_lossy(a1);
    assert!(text.contains("\r\nFrom the beginning"));
    assert!(text.contains("\r\n>From quoted"));
    assert!(!text.contains("X-Mozilla"));

    // Size only, fingerprint only, unknown id: never.
    let other_fp = HeaderFingerprint::new(None, Some("x@example.com"), Some("other"));
    assert_eq!(reuse.lookup("a1@example.com", size, &other_fp), None);
    assert_eq!(reuse.lookup("a1@example.com", size + 1, &fp), None);
    assert_eq!(reuse.lookup("unknown@example.com", size, &fp), None);
    assert_eq!(reuse.lookup("", size, &fp), None);

    // Same Message-ID, size and fingerprint, different content: ambiguous, never.
    let dups = by_id("dup@example.com");
    assert_eq!(dups.len(), 2);
    assert_ne!(dups[0], dups[1]);
    assert_eq!(key(&dups[0]), key(&dups[1]));
    let (size, fp) = key(&dups[0]);
    assert_eq!(reuse.lookup("dup@example.com", size, &fp), None);

    // Same Message-ID, different sizes: each size matches exactly one entry.
    for copy in by_id("dup2@example.com") {
        let (size, fp) = key(&copy);
        assert_eq!(reuse.lookup("dup2@example.com", size, &fp), Some(copy));
    }

    // Identical copies in two folders (Gmail INBOX and All Mail) count as one.
    let gmail = messages(&imap.join("imap.gmail.com").join("INBOX"));
    let (size, fp) = key(&gmail[0].1);
    assert_eq!(
        reuse.lookup("g1@gmail-example.com", size, &fp),
        Some(gmail[0].1.clone())
    );

    // The index is encrypted and bound to the key.
    let header = fs::read(&index_path).unwrap();
    assert_ne!(&header[..16], b"SQLite format 3\0");
    let (size, fp) = key(a1);
    drop(reuse);
    assert!(matches!(
        BodyReuse::open(&index_path, &vault(9)),
        Err(import::Error::ReuseIndexKey(_))
    ));
    let reuse = BodyReuse::open(&index_path, &vault(3)).unwrap();
    assert_eq!(reuse.len().unwrap(), 10);

    // A store rewritten since the scan (compaction) never yields wrong bytes.
    let inbox_path = imap.join("imap.example.com").join("INBOX");
    let mut changed = fs::read(&inbox_path).unwrap();
    changed.drain(..40);
    fs::write(&inbox_path, changed).unwrap();
    assert_eq!(reuse.lookup("a1@example.com", size, &fp), None);

    reuse.discard().unwrap();
    assert!(!index_path.exists());
    env.close();
}

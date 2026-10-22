//! Scan throughput of the body reuse index on a generated synthetic mbox tree.
//!
//! Run with
//! `cargo nextest run -p import --release --run-ignored only perf --no-capture`;
//! `MAZEMAIL_PERF_GB` sets the tree size (default 2). Reports MB/s and peak RSS.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use import::mbox::{Envelope, MboxWriter};
use import::{BodyReuse, MozillaState, NoProgress};
use mazemail_types::Flags;

use crate::common::{Env, vault};

/// Resident set size of this process in KiB, from `ps` (no unsafe needed).
fn rss_kib() -> u64 {
    Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Writes `bytes` worth of mbox files: 8 accounts, folders of varied size, messages
/// of 2–60 KB with Thunderbird headers and some `From ` lines to escape.
fn generate(root: &Path, bytes: u64) -> u64 {
    let filler = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
tempor incididunt ut labore et dolore magna aliqua.\r\n";
    let mut written = 0u64;
    let mut n = 0u64;
    let mut folder = 0u64;
    while written < bytes {
        let account = folder % 8;
        let dir = root
            .join(format!("imap{account}.example.com"))
            .join("INBOX.sbd");
        fs::create_dir_all(&dir).unwrap();
        let file = File::create(dir.join(format!("Folder{folder}"))).unwrap();
        let mut writer = MboxWriter::new(BufWriter::with_capacity(1 << 20, file));
        // Folders from ~1 MB to ~300 MB.
        let target = (1 + (folder * 7919) % 300) << 20;
        let start = writer.bytes_written();
        while writer.bytes_written() - start < target
            && written + writer.bytes_written() - start < bytes
        {
            let size = 2_000 + (n * 104_729) % 58_000;
            let mut body = String::with_capacity(size as usize);
            while (body.len() as u64) < size {
                body.push_str(filler);
            }
            body.push_str("From the archive, quoted\r\n");
            let message = format!(
                "Date: Tue, 01 Oct 2024 10:{:02}:{:02} +0000\r\nFrom: Sender {} <s{}@example.com>\r\n\
                 To: me@example.org\r\nSubject: Message {n}\r\nMessage-ID: <perf-{n}@example.com>\r\n\r\n{body}",
                (n / 60) % 60,
                n % 60,
                n % 1000,
                n % 1000,
            );
            let state = MozillaState::from_flags(Flags::SEEN, &[]);
            writer
                .write_message(
                    &Envelope {
                        sender: None,
                        date: Some(1_727_776_800),
                    },
                    Some(&state),
                    message.as_bytes(),
                )
                .unwrap();
            n += 1;
        }
        written += writer.bytes_written() - start;
        writer.finish().unwrap().flush().unwrap();
        folder += 1;
    }
    n
}

#[test]
#[ignore = "performance measurement, writes a large synthetic tree"]
fn perf() {
    let gb: f64 = std::env::var("MAZEMAIL_PERF_GB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2.0);
    let env = Env::new();
    let tree = env.scratch("ImapMail");
    let t = Instant::now();
    let messages = generate(&tree, (gb * 1e9) as u64);
    println!(
        "generated {messages} messages ({gb} GB) in {:.1?}",
        t.elapsed()
    );

    let reuse = BodyReuse::create(&env.scratch("data").join("reuse.db"), &vault(3)).unwrap();
    let before = rss_kib();
    let peak = AtomicU64::new(before);
    let done = AtomicBool::new(false);
    let stats = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) {
                peak.fetch_max(rss_kib(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let stats = reuse
            .scan(std::slice::from_ref(&tree), &NoProgress)
            .unwrap();
        done.store(true, Ordering::Relaxed);
        stats
    });
    let peak = peak.load(Ordering::Relaxed);
    println!(
        "scanned {} messages, {:.0} MB in {:.1?}: {:.0} MB/s, peak RSS {} MiB (before {} MiB), \
         24 GB would take {:.1} min",
        stats.messages,
        stats.bytes as f64 / 1e6,
        stats.elapsed,
        stats.mb_per_sec(),
        peak / 1024,
        before / 1024,
        24_000.0 / stats.mb_per_sec() / 60.0
    );
    assert_eq!(stats.indexed, messages);
    let t = Instant::now();
    let mut hits = 0;
    for i in (0..messages).step_by((messages / 1000).max(1) as usize) {
        // The size and fingerprint come from the index's own reconstruction, so look
        // up by reading one back: here we only time the miss path plus a few hits.
        if ingest::BodyLookup::lookup(
            &reuse,
            &format!("perf-{i}@example.com"),
            0,
            &ingest::HeaderFingerprint::new(None, None, None),
        )
        .is_some()
        {
            hits += 1;
        }
    }
    println!("1000 lookups (misses) in {:.1?}", t.elapsed());
    assert_eq!(hits, 0);
    env.close();
}

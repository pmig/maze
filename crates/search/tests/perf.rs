//! Spike S7, search half (PLAN §6, §8): index time, index size and search latency over a
//! synthetic corpus, `MmapDirectory` against `EncryptedDirectory` with several block and
//! cache sizes.
//!
//! ```text
//! cargo nextest run -p search --release --run-ignored only --no-capture perf
//! MAZEMAIL_PERF_DOCS=200000 cargo nextest run …   # smaller corpus
//! ```

#![allow(clippy::unwrap_used, clippy::print_stdout)] // a benchmark that reports numbers

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mazemail_types::{AccountId, Address, MessageId, Sensitive, ThreadId};
use search::query::parse;
use search::{
    BlockCache, CommitPolicy, DirectoryOptions, EncryptedDirectory, FacetOptions, IndexDoc,
    IndexOptions, Language, PlanContext, QueryPlan, SearchIndex, SearchOptions, SortOrder,
    TantivyPart,
};
use tantivy::Directory;
use tantivy::directory::{MmapDirectory, TerminatingWrite};
use time::macros::{datetime, offset};
use vault::{BlockCipher, KEY_LEN, KeyId, MasterKey};

const VOCABULARY: usize = 40_000;
const SENDERS: usize = 5_000;
const SYLLABLES: [&str; 24] = [
    "ka", "lo", "mi", "ne", "ru", "ta", "be", "so", "vi", "da", "fe", "gu", "ho", "ji", "pa", "re",
    "si", "to", "ul", "wa", "xe", "yo", "zi", "en",
];
const REAL_WORDS: [&str; 24] = [
    "Rechnung",
    "Rechnungen",
    "Termin",
    "Besprechung",
    "Angebot",
    "Vertrag",
    "Grüße",
    "München",
    "Lieferung",
    "Bestellung",
    "invoice",
    "invoices",
    "meeting",
    "report",
    "quarterly",
    "budget",
    "schedule",
    "project",
    "release",
    "deadline",
    "contract",
    "proposal",
    "offsite",
    "travel",
];

/// Deterministic xorshift64*: no external RNG needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A rank in `0..n` following Zipf's law with exponent 1, like word frequencies in
    /// natural language: P(rank < r) = ln(r + 1) / ln(n).
    fn skewed(&mut self, n: usize) -> usize {
        ((n as f64).powf(self.unit()) as usize).saturating_sub(1) % n
    }
}

struct Corpus {
    words: Vec<String>,
    senders: Vec<Address>,
}

impl Corpus {
    fn new() -> Self {
        let mut words: Vec<String> = REAL_WORDS.iter().map(|w| (*w).to_owned()).collect();
        let mut i = 0usize;
        while words.len() < VOCABULARY {
            let mut word = String::new();
            let mut n = i;
            loop {
                word.push_str(SYLLABLES[n % SYLLABLES.len()]);
                n /= SYLLABLES.len();
                if n == 0 {
                    break;
                }
            }
            if word.len() > 3 {
                words.push(word);
            }
            i += 1;
        }
        let senders = (0..SENDERS)
            .map(|i| {
                Address::new(
                    Some(format!(
                        "{} {}",
                        capitalize(&words[100 + i]),
                        capitalize(&words[9000 + i])
                    )),
                    format!(
                        "{}.{}@{}.example",
                        words[100 + i],
                        words[9000 + i],
                        words[20_000 + i % 500]
                    ),
                )
            })
            .collect();
        Self { words, senders }
    }

    fn text(&self, rng: &mut Rng, words: usize) -> String {
        let mut out = String::with_capacity(words * 8);
        for i in 0..words {
            if i > 0 {
                out.push(if i % 14 == 0 { '.' } else { ' ' });
                if i % 14 == 0 {
                    out.push(' ');
                }
            }
            out.push_str(&self.words[rng.skewed(self.words.len())]);
        }
        out
    }

    fn doc(&self, id: i64, rng: &mut Rng) -> IndexDoc {
        let body_words = 40 + rng.below(320);
        let subject_words = 3 + rng.below(6);
        IndexDoc {
            message_id: MessageId(id),
            thread_id: ThreadId(id / 4),
            account_id: AccountId(1 + (id % 3)),
            date: 1_483_228_800 + rng.below(10 * 365 * 86_400) as i64,
            size: 2_000 + rng.below(200_000) as u64,
            from: vec![self.senders[rng.skewed(SENDERS)].clone()],
            to: vec![self.senders[rng.below(SENDERS)].clone()],
            cc: if rng.below(4) == 0 {
                vec![self.senders[rng.below(SENDERS)].clone()]
            } else {
                Vec::new()
            },
            subject: Sensitive::new(self.text(rng, subject_words)),
            body: Sensitive::new(self.text(rng, body_words)),
            attachments: Vec::new(),
            list_id: None,
            language: match id % 3 {
                0 => Some(Language::De),
                1 => Some(Language::En),
                _ => None,
            },
        }
    }
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn raw_text_len(doc: &IndexDoc) -> u64 {
    let addresses: usize = doc
        .from
        .iter()
        .chain(&doc.to)
        .chain(&doc.cc)
        .map(|a| a.email.len() + a.name.as_ref().map_or(0, String::len))
        .sum();
    (doc.subject.expose().len() + doc.body.expose().len() + addresses) as u64
}

fn dir_size(path: &Path) -> u64 {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum()
}

/// Bytes per file extension (`idx` postings, `pos` positions, `term` dictionary, …).
fn size_breakdown(path: &Path) -> String {
    let mut by_ext: std::collections::BTreeMap<String, u64> = Default::default();
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let ext = name.rsplit('.').next().unwrap_or("").to_owned();
        *by_ext.entry(ext).or_default() += entry.metadata().unwrap().len();
    }
    by_ext
        .into_iter()
        .map(|(ext, bytes)| format!("{ext} {:.1} MiB", bytes as f64 / 1048576.0))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resident set size of this process in MiB (via `ps`, good enough for a benchmark).
fn rss_mib() -> f64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse::<f64>().ok())
        .map_or(f64::NAN, |kib| kib / 1024.0)
}

fn cipher() -> BlockCipher {
    BlockCipher::new(&MasterKey::from_bytes(KeyId::FIRST, [42; KEY_LEN]))
}

/// Builds the index, returning (seconds, raw text bytes).
fn build(index: &SearchIndex, corpus: &Corpus, docs: i64) -> (f64, u64) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    let mut indexer = index
        .indexer(&IndexOptions {
            threads,
            memory_budget: threads * 256 * 1024 * 1024,
            commit: CommitPolicy {
                max_pending: 250_000,
                max_delay: Duration::from_secs(3600),
            },
        })
        .unwrap();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut raw = 0;
    let start = Instant::now();
    for id in 0..docs {
        let doc = corpus.doc(id, &mut rng);
        raw += raw_text_len(&doc);
        indexer.add(&doc).unwrap();
    }
    indexer.finish().unwrap();
    (start.elapsed().as_secs_f64(), raw)
}

/// Re-encrypts a finished plaintext index into an encrypted directory, file by file.
/// The plaintext Tantivy writes is identical either way, so this stands in for indexing
/// the corpus again for every block size.
fn convert(from: &Path, to: &EncryptedDirectory) {
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_owned();
        let name = Path::new(&name);
        let lossy = name.to_string_lossy();
        if lossy.ends_with(".lock") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        if lossy.ends_with(".json") {
            to.atomic_write(name, &bytes).unwrap();
        } else {
            let mut writer = to.open_write(name).unwrap();
            writer.write_all(&bytes).unwrap();
            writer.terminate().unwrap();
        }
    }
}

#[derive(Clone)]
enum Kind {
    Search(SortOrder),
    Threads,
    Facets,
}

struct Workload {
    queries: Vec<(&'static str, String, Kind)>,
}

impl Workload {
    fn new(corpus: &Corpus) -> Self {
        let mut rng = Rng(7);
        let mut queries = Vec::new();
        let word =
            |rng: &mut Rng, lo: usize, hi: usize| corpus.words[lo + rng.below(hi - lo)].clone();
        for _ in 0..20 {
            queries.push((
                "common word",
                word(&mut rng, 24, 60),
                Kind::Search(SortOrder::Relevance),
            ));
            queries.push((
                "mid word",
                word(&mut rng, 500, 3000),
                Kind::Search(SortOrder::Relevance),
            ));
            queries.push((
                "rare word",
                word(&mut rng, 20_000, VOCABULARY),
                Kind::Search(SortOrder::Relevance),
            ));
            queries.push((
                "two words",
                format!("{} {}", word(&mut rng, 24, 400), word(&mut rng, 24, 400)),
                Kind::Search(SortOrder::Relevance),
            ));
            queries.push((
                "phrase",
                format!("\"{} {}\"", word(&mut rng, 24, 80), word(&mut rng, 24, 80)),
                Kind::Search(SortOrder::Relevance),
            ));
            queries.push((
                "from:",
                format!("from:{}", corpus.words[100 + rng.below(200)]),
                Kind::Search(SortOrder::NewestFirst),
            ));
            queries.push((
                "word + date",
                format!("{} after:2024-01-01", word(&mut rng, 100, 1000)),
                Kind::Search(SortOrder::NewestFirst),
            ));
            queries.push((
                "a OR b",
                format!(
                    "{} OR {}",
                    word(&mut rng, 200, 2000),
                    word(&mut rng, 200, 2000)
                ),
                Kind::Search(SortOrder::Relevance),
            ));
            queries.push((
                "common, newest",
                word(&mut rng, 24, 60),
                Kind::Search(SortOrder::NewestFirst),
            ));
            queries.push((
                "stemmed DE",
                "Rechnungen".to_owned(),
                Kind::Search(SortOrder::Relevance),
            ));
            queries.push(("threads", word(&mut rng, 500, 3000), Kind::Threads));
            queries.push(("facets", word(&mut rng, 500, 3000), Kind::Facets));
        }
        Self { queries }
    }
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Runs the workload once cold (fresh process state for this index) and once warm;
/// returns (cold p95, warm p50, warm p95, warm max, per-kind warm p95).
fn run(
    index: &SearchIndex,
    workload: &Workload,
) -> (
    Duration,
    Duration,
    Duration,
    Duration,
    Vec<(&'static str, Duration)>,
) {
    let context = PlanContext::new(datetime!(2026-06-01 00:00 UTC), offset!(UTC));
    let parts: Vec<(&'static str, TantivyPart, Kind)> = workload
        .queries
        .iter()
        .map(|(label, query, kind)| {
            let plan = QueryPlan::new(&parse(query).unwrap().query, &context).unwrap();
            (*label, plan.text.unwrap(), kind.clone())
        })
        .collect();
    let pass = |record: bool| {
        let mut all = Vec::new();
        let mut by_kind: Vec<(&'static str, Vec<Duration>)> = Vec::new();
        for (label, part, kind) in &parts {
            let searcher = index.searcher();
            let start = Instant::now();
            match kind {
                Kind::Search(order) => {
                    let hits = searcher
                        .search(
                            part,
                            &SearchOptions {
                                limit: 50,
                                order: *order,
                                ..SearchOptions::default()
                            },
                        )
                        .unwrap();
                    std::hint::black_box(hits);
                }
                Kind::Threads => {
                    let threads = searcher
                        .search_threads(
                            part,
                            &SearchOptions {
                                limit: 50,
                                ..SearchOptions::default()
                            },
                        )
                        .unwrap();
                    std::hint::black_box(threads);
                }
                Kind::Facets => {
                    let facets = searcher.facets(part, &FacetOptions::default()).unwrap();
                    std::hint::black_box(facets);
                }
            }
            let elapsed = start.elapsed();
            all.push(elapsed);
            if record {
                match by_kind.iter_mut().find(|(k, _)| k == label) {
                    Some((_, list)) => list.push(elapsed),
                    None => by_kind.push((label, vec![elapsed])),
                }
            }
        }
        all.sort();
        (all, by_kind)
    };
    let (cold, _) = pass(false);
    let (warm, by_kind) = pass(true);
    let per_kind = by_kind
        .into_iter()
        .map(|(label, mut list)| {
            list.sort();
            (label, percentile(&list, 0.95))
        })
        .collect();
    (
        percentile(&cold, 0.95),
        percentile(&warm, 0.5),
        percentile(&warm, 0.95),
        *warm.last().unwrap(),
        per_kind,
    )
}

#[test]
#[ignore = "benchmark: minutes on a 1M corpus, run with --release --run-ignored only"]
fn perf_search_index_s7() {
    let docs: i64 = std::env::var("MAZEMAIL_PERF_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let corpus = Corpus::new();
    let workload = Workload::new(&corpus);
    let root = tempfile::tempdir().unwrap();
    let mut out = std::io::stdout().lock();
    writeln!(out, "\n== S7 search: {docs} synthetic messages ==").unwrap();

    // Plaintext baseline.
    let mmap_path = root.path().join("mmap");
    std::fs::create_dir_all(&mmap_path).unwrap();
    let plain = SearchIndex::open(MmapDirectory::open(&mmap_path).unwrap()).unwrap();
    let (seconds, raw) = build(&plain, &corpus, docs);
    let plain_size = dir_size(&mmap_path);
    writeln!(
        out,
        "raw text {:.1} MiB; MmapDirectory: index {seconds:.1} s ({:.0} docs/s), size {:.1} MiB = {:.1} % of raw text",
        raw as f64 / 1048576.0,
        docs as f64 / seconds,
        plain_size as f64 / 1048576.0,
        plain_size as f64 * 100.0 / raw as f64
    )
    .unwrap();
    writeln!(out, "  by file type: {}", size_breakdown(&mmap_path)).unwrap();

    // Encrypted, indexed directly (16 KiB blocks).
    let enc_path = root.path().join("enc16");
    let cache = Arc::new(BlockCache::new(32 << 20));
    let encrypted = SearchIndex::open_encrypted(
        &enc_path,
        cipher(),
        DirectoryOptions {
            block_size: 16 << 10,
            cache: Arc::clone(&cache),
        },
    )
    .unwrap();
    let (enc_seconds, _) = build(&encrypted, &corpus, docs);
    let enc_size = dir_size(&enc_path);
    writeln!(
        out,
        "EncryptedDirectory 16 KiB: index {enc_seconds:.1} s ({:+.1} % vs mmap), size {:.1} MiB ({:+.2} % vs mmap)",
        (enc_seconds / seconds - 1.0) * 100.0,
        enc_size as f64 / 1048576.0,
        (enc_size as f64 / plain_size as f64 - 1.0) * 100.0
    )
    .unwrap();
    drop(encrypted);

    writeln!(
        out,
        "\n{:<34} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "directory", "cold p95", "warm p50", "warm p95", "warm max", "+RSS"
    )
    .unwrap();
    let report =
        |out: &mut std::io::StdoutLock<'_>, label: &str, index: &SearchIndex, rss_before: f64| {
            let (cold, p50, p95, max, per_kind) = run(index, &workload);
            writeln!(
                out,
                "{label:<34} {:>8.1}ms {:>8.1}ms {:>8.1}ms {:>8.1}ms {:>7.0}MiB",
                ms(cold),
                ms(p50),
                ms(p95),
                ms(max),
                rss_mib() - rss_before
            )
            .unwrap();
            per_kind
        };
    drop(plain);
    let rss_before = rss_mib();
    let plain = SearchIndex::open(MmapDirectory::open(&mmap_path).unwrap()).unwrap();
    let mmap_kinds = report(&mut out, "MmapDirectory", &plain, rss_before);
    drop(plain);

    let mut kinds_16k = Vec::new();
    for (block_kib, cache_mib) in [
        (4u32, 32u64),
        (16, 8),
        (16, 32),
        (16, 128),
        (64, 32),
        (64, 128),
    ] {
        let path = root.path().join(format!("enc{block_kib}"));
        if block_kib != 16 {
            let target = EncryptedDirectory::open(
                &path,
                cipher(),
                DirectoryOptions {
                    block_size: block_kib << 10,
                    cache: Arc::new(BlockCache::new(1 << 20)),
                },
            )
            .unwrap();
            if !path.join("meta.json").exists() {
                convert(&mmap_path, &target);
            }
        }
        let rss_before = rss_mib();
        let cache = Arc::new(BlockCache::new(cache_mib << 20));
        let index = SearchIndex::open_encrypted(
            &path,
            cipher(),
            DirectoryOptions {
                block_size: block_kib << 10,
                cache: Arc::clone(&cache),
            },
        )
        .unwrap();
        let label = format!("Encrypted {block_kib} KiB, cache {cache_mib} MiB");
        let kinds = report(&mut out, &label, &index, rss_before);
        let stats = cache.stats();
        writeln!(
            out,
            "{:<34} cache hits {} misses {} ({:.0} % hit), {:.1} MiB held",
            "",
            stats.hits,
            stats.misses,
            stats.hits as f64 * 100.0 / (stats.hits + stats.misses).max(1) as f64,
            stats.bytes as f64 / 1048576.0
        )
        .unwrap();
        if block_kib == 16 && cache_mib == 32 {
            kinds_16k = kinds;
        }
    }

    writeln!(
        out,
        "\nwarm p95 per query kind: MmapDirectory vs Encrypted 16 KiB / 32 MiB"
    )
    .unwrap();
    for ((label, mmap), (_, enc)) in mmap_kinds.iter().zip(&kinds_16k) {
        writeln!(
            out,
            "  {label:<16} {:>8.2}ms {:>8.2}ms",
            ms(*mmap),
            ms(*enc)
        )
        .unwrap();
    }
}

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use proptest::prelude::*;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{Lock, TerminatingWrite, WatchCallback};
use tantivy::{Directory, HasLen};
use vault::{BlockCipher, KEY_LEN, KeyId, MasterKey};

use super::*;

fn cipher(seed: u8) -> BlockCipher {
    BlockCipher::new(&MasterKey::from_bytes(KeyId::FIRST, [seed; KEY_LEN]))
}

fn options(block_size: u32) -> DirectoryOptions {
    DirectoryOptions {
        block_size,
        cache: Arc::new(BlockCache::new(1024 * 1024)),
    }
}

fn open(root: &Path, block_size: u32) -> EncryptedDirectory {
    EncryptedDirectory::open(root, cipher(7), options(block_size)).unwrap()
}

fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn write_file(dir: &EncryptedDirectory, name: &str, content: &[u8], chunk: usize) {
    let mut writer = dir.open_write(Path::new(name)).unwrap();
    for piece in content.chunks(chunk.max(1)) {
        writer.write_all(piece).unwrap();
    }
    writer.terminate().unwrap();
}

#[test]
fn files_round_trip_across_block_boundaries() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    for (i, len) in [0usize, 1, 479, 480, 481, 512, 1000, 5000]
        .into_iter()
        .enumerate()
    {
        let name = format!("f{i}.idx");
        let content = data(len, i as u8);
        write_file(&dir, &name, &content, 97);
        let slice = dir.open_read(Path::new(&name)).unwrap();
        assert_eq!(slice.len(), len);
        assert_eq!(
            slice.read_bytes().unwrap().as_slice(),
            &content[..],
            "len {len}"
        );
    }
}

#[test]
fn empty_ranges_and_out_of_bounds_reads() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    write_file(&dir, "a", &data(100, 1), 100);
    let handle = dir.get_file_handle(Path::new("a")).unwrap();
    assert!(handle.read_bytes(10..10).unwrap().is_empty());
    assert!(handle.read_bytes(90..101).is_err());
}

#[test]
fn missing_and_existing_files() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    let path = Path::new("seg.idx");
    assert!(!dir.exists(path).unwrap());
    assert!(matches!(
        dir.open_read(path),
        Err(OpenReadError::FileDoesNotExist(_))
    ));
    assert!(matches!(
        dir.atomic_read(Path::new("meta.json")),
        Err(OpenReadError::FileDoesNotExist(_))
    ));
    write_file(&dir, "seg.idx", b"x", 1);
    assert!(dir.exists(path).unwrap());
    assert!(matches!(
        dir.open_write(path),
        Err(OpenWriteError::FileAlreadyExists(_))
    ));
    dir.delete(path).unwrap();
    assert!(matches!(
        dir.delete(path),
        Err(DeleteError::FileDoesNotExist(_))
    ));
}

#[test]
fn paths_cannot_escape_the_directory() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    for bad in ["../x", "/etc/passwd", "a/../../b", ""] {
        assert!(dir.open_write(Path::new(bad)).is_err(), "{bad}");
        assert!(dir.atomic_write(Path::new(bad), b"x").is_err(), "{bad}");
    }
}

#[test]
fn atomic_files_round_trip_and_replace() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    let path = Path::new("meta.json");
    dir.atomic_write(path, b"{\"v\":1}").unwrap();
    assert_eq!(dir.atomic_read(path).unwrap(), b"{\"v\":1}");
    let big = data(3000, 9);
    dir.atomic_write(path, &big).unwrap();
    assert_eq!(dir.atomic_read(path).unwrap(), big);
    // No temporary files left behind.
    let names: Vec<_> = std::fs::read_dir(temp.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("meta.json")]);
}

#[test]
fn wrong_key_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    write_file(&dir, "seg.idx", &data(2000, 3), 2000);
    dir.atomic_write(Path::new("meta.json"), b"{}").unwrap();
    let stranger = EncryptedDirectory::open(temp.path(), cipher(8), options(512)).unwrap();
    assert!(stranger.open_read(Path::new("seg.idx")).is_err());
    assert!(stranger.atomic_read(Path::new("meta.json")).is_err());
}

fn read_all(dir: &EncryptedDirectory, name: &str) -> Result<Vec<u8>, String> {
    let slice = dir.open_read(Path::new(name)).map_err(|e| e.to_string())?;
    let bytes = slice.read_bytes().map_err(|e| e.to_string())?;
    Ok(bytes.as_slice().to_vec())
}

#[test]
fn every_tampered_byte_is_detected() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    let content = data(1200, 5);
    write_file(&dir, "seg.idx", &content, 1200);
    let path = temp.path().join("seg.idx");
    let original = std::fs::read(&path).unwrap();
    for position in (0..original.len()).step_by(7) {
        let mut bad = original.clone();
        bad[position] ^= 0x10;
        std::fs::write(&path, &bad).unwrap();
        // A fresh cache, so nothing decrypted earlier masks the change.
        let fresh = open(temp.path(), 512);
        let result = read_all(&fresh, "seg.idx");
        assert!(result.is_err(), "byte {position}");
    }
    // Truncation at a block boundary.
    std::fs::write(&path, &original[..original.len() - 100]).unwrap();
    let fresh = open(temp.path(), 512);
    let result = read_all(&fresh, "seg.idx");
    assert!(result.is_err());
}

#[test]
fn swapped_files_are_detected() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    write_file(&dir, "a.idx", &data(700, 1), 700);
    write_file(&dir, "b.idx", &data(700, 2), 700);
    std::fs::copy(temp.path().join("a.idx"), temp.path().join("b.idx")).unwrap();
    let fresh = open(temp.path(), 512);
    assert!(fresh.open_read(Path::new("a.idx")).is_ok());
    assert!(fresh.open_read(Path::new("b.idx")).is_err());

    dir.atomic_write(Path::new("meta.json"), b"{}").unwrap();
    dir.atomic_write(Path::new(".managed.json"), b"[]").unwrap();
    std::fs::copy(
        temp.path().join(".managed.json"),
        temp.path().join("meta.json"),
    )
    .unwrap();
    assert!(dir.atomic_read(Path::new("meta.json")).is_err());
}

#[test]
fn files_contain_no_plaintext() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    let secret = b"zebrafinch-quintessential-marmalade ".repeat(50);
    write_file(&dir, "seg.idx", &secret, 333);
    dir.atomic_write(Path::new("meta.json"), &secret).unwrap();
    for entry in std::fs::read_dir(temp.path()).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        assert!(
            !bytes.windows(10).any(|window| window == b"zebrafinch"),
            "plaintext found on disk"
        );
    }
}

#[test]
fn reads_go_through_the_shared_cache() {
    let temp = tempfile::tempdir().unwrap();
    let cache = Arc::new(BlockCache::new(1024 * 1024));
    let dir = EncryptedDirectory::open(
        temp.path(),
        cipher(7),
        DirectoryOptions {
            block_size: 512,
            cache: Arc::clone(&cache),
        },
    )
    .unwrap();
    write_file(&dir, "seg.idx", &data(12_000, 1), 4000);
    let handle = dir.get_file_handle(Path::new("seg.idx")).unwrap();
    let before = cache.stats();
    handle.read_bytes(600..700).unwrap();
    handle.read_bytes(610..690).unwrap();
    let after = cache.stats();
    assert_eq!(after.misses - before.misses, 1);
    assert_eq!(after.hits - before.hits, 1);
    // A large read (more than 16 blocks) is cached as one range.
    let blocks = after.blocks;
    handle.read_bytes(0..12_000).unwrap();
    handle.read_bytes(0..12_000).unwrap();
    let stats = cache.stats();
    assert_eq!(stats.blocks, blocks + 1);
    assert_eq!(stats.hits - after.hits, 1);

    // Unless it is too large for the cache.
    let small = Arc::new(BlockCache::new(16 * 1024));
    let dir = EncryptedDirectory::open(
        temp.path(),
        cipher(7),
        DirectoryOptions {
            block_size: 512,
            cache: Arc::clone(&small),
        },
    )
    .unwrap();
    let handle = dir.get_file_handle(Path::new("seg.idx")).unwrap();
    let blocks = small.stats().blocks;
    assert_eq!(
        handle.read_bytes(0..12_000).unwrap().as_slice(),
        &data(12_000, 1)[..]
    );
    assert_eq!(small.stats().blocks, blocks);
}

#[test]
fn locks_are_exclusive_and_released_on_drop() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    let lock = Lock {
        filepath: ".tantivy-writer.lock".into(),
        is_blocking: false,
    };
    let held = dir.acquire_lock(&lock).unwrap();
    assert!(matches!(dir.acquire_lock(&lock), Err(LockError::LockBusy)));
    drop(held);
    let again = dir.acquire_lock(&lock).unwrap();
    drop(again);
}

#[test]
fn watch_fires_on_meta_json_writes() {
    let temp = tempfile::tempdir().unwrap();
    let dir = open(temp.path(), 512);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let handle = dir
        .watch(WatchCallback::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }))
        .unwrap();
    dir.atomic_write(Path::new("managed.json"), b"[]").unwrap();
    dir.atomic_write(Path::new("meta.json"), b"{}").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(handle);
}

#[test]
fn invalid_block_size_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    assert!(EncryptedDirectory::open(temp.path(), cipher(1), options(100)).is_err());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Random contents written in random chunks read back identically over random
    /// ranges, for every supported block size and with or without a cache.
    #[test]
    fn random_writes_and_ranges_round_trip(
        len in 0usize..40_000,
        chunk in 1usize..5000,
        block_size in prop::sample::select(vec![512u32, 4096, 16384]),
        cache_bytes in prop::sample::select(vec![0u64, 8192, 1 << 20]),
        ranges in prop::collection::vec((any::<prop::sample::Index>(), any::<prop::sample::Index>()), 1..12),
        seed in any::<u8>(),
    ) {
        let temp = tempfile::tempdir().unwrap();
        let dir = EncryptedDirectory::open(temp.path(), cipher(3), DirectoryOptions {
            block_size,
            cache: Arc::new(BlockCache::new(cache_bytes)),
        }).unwrap();
        let content = data(len, seed);
        write_file(&dir, "p.idx", &content, chunk);
        let handle = dir.get_file_handle(Path::new("p.idx")).unwrap();
        prop_assert_eq!(handle.len(), len);
        for (a, b) in ranges {
            let (a, b) = (a.index(len + 1), b.index(len + 1));
            let range = a.min(b)..a.max(b);
            let bytes = handle.read_bytes(range.clone()).unwrap();
            prop_assert_eq!(bytes.as_slice(), &content[range]);
        }
    }
}

//! Shared helpers.

use std::fs;
use std::path::{Path, PathBuf};

use ingest::Ingestor;
use store::{Store, StoreConfig};
use vault::{KEY_LEN, KeyId, MasterKey, Vault};

pub(crate) fn vault(seed: u8) -> Vault {
    Vault::from_master_key(MasterKey::from_bytes(KeyId::FIRST, [seed; KEY_LEN]))
}

/// A store and an ingestor in a temp directory.
pub(crate) struct Env {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) store: Store,
    pub(crate) ingestor: Ingestor,
}

impl Env {
    pub(crate) fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = StoreConfig::new(dir.path().join("data"));
        config.gc_interval = None;
        let store = Store::open(&config, &vault(3)).unwrap();
        let ingestor = Ingestor::new(store.clone());
        Self {
            dir,
            store,
            ingestor,
        }
    }

    /// A scratch directory inside the environment.
    pub(crate) fn scratch(&self, name: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::create_dir_all(&path).unwrap();
        path
    }

    pub(crate) fn close(self) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(self.store.close())
            .unwrap();
    }
}

/// The fixture directory.
pub(crate) fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Copies a directory tree.
pub(crate) fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A copy of the fake Thunderbird root (with `profiles.ini`) and the profile path,
/// plus an address book created like Thunderbird does.
pub(crate) fn thunderbird_copy(env: &Env) -> (PathBuf, PathBuf) {
    let root = env.scratch("thunderbird");
    copy_tree(&fixtures().join("thunderbird"), &root);
    let profile = root.join("Profiles").join("test.default-release");
    create_address_book(&profile.join("abook.sqlite"));
    (root, profile)
}

/// An address book with Thunderbird's schema (`SQLiteDirectory.sys.mjs`).
pub(crate) fn create_address_book(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE properties (card TEXT, name TEXT, value TEXT);
         CREATE TABLE lists (uid TEXT PRIMARY KEY, name TEXT, nickName TEXT, description TEXT);
         CREATE TABLE list_cards (list TEXT, card TEXT, PRIMARY KEY(list, card));
         CREATE INDEX properties_card ON properties(card);
         CREATE INDEX properties_name ON properties(name);
         INSERT INTO properties VALUES
           ('c1', 'DisplayName', 'Ben Example'),
           ('c1', 'PrimaryEmail', 'ben@example.org'),
           ('c1', 'SecondEmail', 'ben.private@example.net'),
           ('c1', '_vCard', 'BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Ben Example\r\nEMAIL;PREF=1:ben@example.org\r\nEMAIL:ben.private@example.net\r\nEND:VCARD\r\n'),
           ('c2', 'FirstName', 'Carla'),
           ('c2', 'LastName', 'Muster'),
           ('c2', 'PrimaryEmail', 'carla@example.net'),
           ('c3', '_vCard', 'BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Dora Vcard\r\nEMAIL:dora@example.com\r\nEND:VCARD\r\n'),
           ('c4', 'DisplayName', 'No Address');
         INSERT INTO lists VALUES ('l1', 'Team', 'team', 'The team');
         INSERT INTO list_cards VALUES ('l1', 'c1'), ('l1', 'c2');",
    )
    .unwrap();
    // Keep the WAL around, as a running Thunderbird would.
    std::mem::forget(conn);
}

/// Replaces the temp profile path in serialized output and sorts object keys, so
/// snapshots are stable (serde_json may preserve insertion order when another crate
/// in the build enables `preserve_order`).
pub(crate) fn redact(value: serde_json::Value, prefix: &Path) -> serde_json::Value {
    let prefix_text = prefix.to_string_lossy().into_owned();
    match value {
        serde_json::Value::String(s) => {
            serde_json::Value::String(s.replace(&prefix_text, "[profile]"))
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(|v| redact(v, prefix)).collect())
        }
        serde_json::Value::Object(o) => {
            let mut entries: Vec<(String, serde_json::Value)> = o.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, redact(v, prefix)))
                    .collect(),
            )
        }
        other => other,
    }
}

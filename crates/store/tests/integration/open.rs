//! Opening, keys, the downgrade guard, persistence.

use std::fs;

use store::{AccountKind, Error, Queries, SCHEMA_VERSION, Store};

use crate::common::{self, config, vault};

#[test]
fn creates_the_layout_and_reopens() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    assert_eq!(store.migration().from, 0);
    assert_eq!(store.migration().to, SCHEMA_VERSION);
    assert_eq!(store.migration().backup, None);
    if cfg!(target_vendor = "apple") {
        assert_eq!(store.cipher_provider(), "commoncrypto");
    } else {
        assert_eq!(store.cipher_provider(), "openssl");
    }
    let fixture = common::fixture(&store, AccountKind::Imap);
    common::close(store);

    for dir in ["blobs", "cache", "backups", "tmp"] {
        assert!(tmp.path().join("mail").join(dir).is_dir(), "{dir}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(tmp.path().join("mail"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    let store = common::open(tmp.path());
    assert_eq!(store.migration().from, SCHEMA_VERSION);
    let reader = store.reader().unwrap();
    let accounts = reader.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].id, fixture.account);
    assert_eq!(reader.folders(Some(fixture.account)).unwrap().len(), 3);
    // Thunderbird's default tags are seeded.
    assert_eq!(reader.tags().unwrap().len(), 5);
}

#[test]
fn wrong_key_is_a_typed_error_and_creates_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    common::fixture(&store, AccountKind::Imap);
    common::close(store);
    let db = tmp.path().join("mail/mazemail.db");
    let before = fs::read(&db).unwrap();

    let err = Store::open(&config(tmp.path()), &vault(2)).unwrap_err();
    assert!(
        matches!(err, Error::WrongKey { ref path } if *path == db),
        "{err:?}"
    );
    assert_eq!(
        fs::read(&db).unwrap(),
        before,
        "the database must stay untouched"
    );

    // The right key still works.
    let store = common::open(tmp.path());
    assert_eq!(store.reader().unwrap().accounts().unwrap().len(), 1);
}

#[test]
fn a_plaintext_database_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("mail")).unwrap();
    let db = tmp.path().join("mail/mazemail.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE x (y);")
        .unwrap();
    let err = Store::open(&config(tmp.path()), &vault(1)).unwrap_err();
    assert!(matches!(err, Error::PlaintextDatabase { .. }), "{err:?}");
}

#[test]
fn downgrade_guard_refuses_a_newer_schema() {
    let tmp = tempfile::tempdir().unwrap();
    common::close(common::open(tmp.path()));

    // Simulate a newer Maze: bump user_version with the right key.
    let db = tmp.path().join("mail/mazemail.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let key = vault(1).sqlcipher_pragma_key();
    conn.execute_batch(&format!("PRAGMA key = \"{}\";", key.expose_secret()))
        .unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);

    let err = Store::open(&config(tmp.path()), &vault(1)).unwrap_err();
    assert!(
        matches!(err, Error::SchemaTooNew { found, supported }
            if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION),
        "{err:?}"
    );
    assert!(err.to_string().contains("update Maze"));
}

#[test]
fn writes_after_close_fail_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let other = store.clone();
    common::close(store);
    let err = other
        .write_blocking(|w| w.audit("user", "noop", None))
        .unwrap_err();
    assert!(matches!(err, Error::Closed), "{err:?}");
    assert!(matches!(other.reader(), Err(Error::Closed)));
}

#[test]
fn accounts_keep_their_uuid() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let a = common::fixture(&store, AccountKind::Imap).account;
    let b = common::fixture(&store, AccountKind::Gmail).account;
    let uuid_of = |store: &Store, id| store.reader().unwrap().account(id).unwrap().unwrap().uuid;
    let (ua, ub) = (uuid_of(&store, a), uuid_of(&store, b));
    assert_ne!(ua, ub);
    assert_eq!(ua.get_version_num(), 4);
    store
        .write_blocking(move |w| w.update_account(a, "Renamed", "{}", 3))
        .unwrap();
    common::close(store);

    let store = common::open(tmp.path());
    assert_eq!(uuid_of(&store, a), ua);
    let reader = store.reader().unwrap();
    assert_eq!(reader.account_by_uuid(ub).unwrap().unwrap().id, b);
    assert_eq!(reader.account_by_uuid(store::Uuid::nil()).unwrap(), None);
    drop(reader);
    common::close(store);
}

//! Contacts synced from a server (schema v13): people and their `contact` rows,
//! shared addresses, full-sync cleanup, sync tokens, photos, and removal with the
//! account.

use store::{AccountKind, Queries as _, ServerContact, Store};

use crate::common::{fixture, open};

const BOOK: &str = "google:6f000000-0000-4000-8000-000000000001";

fn person(id: &str, name: &str, emails: &[&str]) -> ServerContact {
    ServerContact {
        resource_name: format!("people/{id}"),
        etag: Some(format!("etag-{id}")),
        display_name: Some(name.to_owned()),
        emails: emails.iter().map(|e| (*e).to_owned()).collect(),
        vcard: format!("BEGIN:VCARD\r\nVERSION:4.0\r\nFN:{name}\r\nEND:VCARD\r\n"),
        photo_url: None,
    }
}

fn upsert(store: &Store, person: ServerContact, seen: i64) {
    store
        .write_blocking(move |w| w.upsert_server_contact(BOOK, &person, seen))
        .unwrap();
}

/// `(email, owner)` of the book's rows, by email.
fn rows(store: &Store) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = store
        .reader()
        .unwrap()
        .search_contacts("", 100)
        .unwrap()
        .into_iter()
        .filter(|c| c.book == BOOK)
        .map(|c| {
            let owner = store
                .reader()
                .unwrap()
                .server_contact_by_email(BOOK, &c.email)
                .unwrap()
                .map(|p| p.resource_name)
                .unwrap_or_default();
            (c.email, owner)
        })
        .collect();
    rows.sort();
    rows
}

fn pair(email: &str, owner: &str) -> (String, String) {
    (email.to_owned(), format!("people/{owner}"))
}

#[test]
fn people_become_one_row_per_address() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let account = fixture(&store, AccountKind::Gmail).account;
    store
        .write_blocking(move |w| w.ensure_contact_source(BOOK, account, "google"))
        .unwrap();
    upsert(
        &store,
        person(
            "c1",
            "Anna",
            &["Anna@Example.com", "anna@work.example", "ANNA@example.com"],
        ),
        1,
    );
    upsert(&store, person("c2", "Nobody", &[]), 1);
    assert_eq!(
        rows(&store),
        [
            pair("Anna@Example.com", "c1"),
            pair("anna@work.example", "c1")
        ]
    );
    let reader = store.reader().unwrap();
    let stored = reader.server_contact(BOOK, "people/c1").unwrap().unwrap();
    assert_eq!(stored.etag.as_deref(), Some("etag-c1"));
    assert_eq!(stored.emails, ["anna@example.com", "anna@work.example"]);
    assert!(stored.vcard.contains("FN:Anna"));
    let contact = reader
        .contact_by_email("anna@example.com")
        .unwrap()
        .unwrap();
    assert_eq!(contact.book, BOOK);
    assert_eq!(contact.display_name.as_deref(), Some("Anna"));
    assert!(contact.vcard.unwrap().contains("FN:Anna"));
    // People without an address are kept (for the Address Book), without rows.
    assert_eq!(reader.server_contacts(BOOK).unwrap().len(), 2);
    drop(reader);

    // An address leaves, another arrives.
    upsert(
        &store,
        person("c1", "Anna E.", &["anna@example.com", "anna@new.example"]),
        2,
    );
    assert_eq!(
        rows(&store),
        [
            pair("anna@example.com", "c1"),
            pair("anna@new.example", "c1")
        ]
    );
    // Deleting removes the rows.
    assert!(
        store
            .write_blocking(|w| w.delete_server_contact(BOOK, "people/c1"))
            .unwrap()
    );
    assert!(
        !store
            .write_blocking(|w| w.delete_server_contact(BOOK, "people/c1"))
            .unwrap()
    );
    assert!(rows(&store).is_empty());
}

#[test]
fn a_shared_address_moves_to_the_other_person() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let account = fixture(&store, AccountKind::Gmail).account;
    store
        .write_blocking(move |w| w.ensure_contact_source(BOOK, account, "google"))
        .unwrap();
    upsert(&store, person("c1", "Office", &["office@example.com"]), 1);
    upsert(
        &store,
        person("c2", "Bob", &["office@example.com", "bob@example.com"]),
        1,
    );
    // The first one keeps it.
    assert_eq!(
        rows(&store),
        [
            pair("bob@example.com", "c2"),
            pair("office@example.com", "c1")
        ]
    );
    // c1 drops the address: it goes to c2.
    upsert(&store, person("c1", "Office", &["front@example.com"]), 2);
    assert_eq!(
        rows(&store),
        [
            pair("bob@example.com", "c2"),
            pair("front@example.com", "c1"),
            pair("office@example.com", "c2")
        ]
    );
    // c2 is deleted: c1 doesn't have it any more, so it's gone.
    store
        .write_blocking(|w| w.delete_server_contact(BOOK, "people/c2"))
        .unwrap();
    assert_eq!(rows(&store), [pair("front@example.com", "c1")]);
}

#[test]
fn a_full_sync_removes_people_it_didnt_see_and_keeps_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let account = fixture(&store, AccountKind::Gmail).account;
    store
        .write_blocking(move |w| w.ensure_contact_source(BOOK, account, "google"))
        .unwrap();
    upsert(&store, person("c1", "Anna", &["anna@example.com"]), 100);
    upsert(&store, person("c2", "Bob", &["bob@example.com"]), 100);
    store
        .write_blocking(|w| w.set_contact_sync_token(BOOK, Some("sync-1"), true))
        .unwrap();
    let source = store
        .reader()
        .unwrap()
        .contact_source(BOOK)
        .unwrap()
        .unwrap();
    assert_eq!(source.sync_token.as_deref(), Some("sync-1"));
    assert_eq!(source.account, account);
    assert_eq!(source.state, "ok");
    assert!(source.last_full_sync.is_some() && source.last_sync.is_some());

    // The next full run (200) only sees Bob.
    upsert(&store, person("c2", "Bob", &["bob@example.com"]), 200);
    let removed = store
        .write_blocking(|w| w.finish_full_contact_sync(BOOK, 200))
        .unwrap();
    assert_eq!(removed, 1);
    assert_eq!(rows(&store), [pair("bob@example.com", "c2")]);

    // Missing permission forgets the token.
    store
        .write_blocking(|w| {
            w.set_contact_source_error(BOOK, "missing_scope", "contacts need permission")
        })
        .unwrap();
    let source = store
        .reader()
        .unwrap()
        .contact_source(BOOK)
        .unwrap()
        .unwrap();
    assert_eq!(source.state, "missing_scope");
    assert_eq!(source.sync_token, None);
    assert!(
        store
            .write_blocking(|w| w.set_contact_source_error(BOOK, "weird", "x"))
            .is_err()
    );
}

#[test]
fn photos_are_remembered_and_go_with_the_books() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let account = fixture(&store, AccountKind::Gmail).account;
    store
        .write_blocking(move |w| w.ensure_contact_source(BOOK, account, "google"))
        .unwrap();
    let mut anna = person("c1", "Anna", &["anna@example.com"]);
    anna.photo_url = Some("https://photos.example/anna=s96".to_owned());
    upsert(&store, anna, 1);
    upsert(&store, person("c2", "Bob", &["bob@example.com"]), 1);
    let reader = store.reader().unwrap();
    assert_eq!(
        reader
            .contact_photo_url("ANNA@example.com", None)
            .unwrap()
            .as_deref(),
        Some("https://photos.example/anna=s96")
    );
    assert_eq!(
        reader.contact_photo_url("bob@example.com", None).unwrap(),
        None
    );
    assert_eq!(
        reader.contact_photos_due(i64::MAX, 10).unwrap(),
        ["https://photos.example/anna=s96"]
    );
    drop(reader);
    store
        .write_blocking(|w| w.record_contact_photo("https://photos.example/anna=s96", true))
        .unwrap();
    let reader = store.reader().unwrap();
    let state = reader
        .contact_photo("https://photos.example/anna=s96")
        .unwrap()
        .unwrap();
    assert!(state.found);
    // Fetched now: due only for a later cutoff.
    assert!(
        reader
            .contact_photos_due(state.fetched_at, 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reader
            .contact_photos_due(state.fetched_at + 1, 10)
            .unwrap()
            .len(),
        1
    );
    drop(reader);

    // Disconnecting removes the books, the people, their rows and the photo record.
    let urls = store
        .write_blocking(move |w| w.delete_contact_books(account))
        .unwrap();
    assert_eq!(urls, ["https://photos.example/anna=s96"]);
    let reader = store.reader().unwrap();
    assert!(reader.contact_sources(account).unwrap().is_empty());
    assert!(reader.server_contacts(BOOK).unwrap().is_empty());
    assert!(reader.search_contacts("", 10).unwrap().is_empty());
    assert_eq!(
        reader
            .contact_photo("https://photos.example/anna=s96")
            .unwrap(),
        None
    );
}

#[test]
fn removing_the_account_removes_its_contacts() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let account = fixture(&store, AccountKind::Gmail).account;
    store
        .write_blocking(move |w| {
            w.ensure_contact_source(BOOK, account, "google")?;
            w.bump_contact("collected@example.com", None, 1)?;
            Ok(())
        })
        .unwrap();
    let mut changes = store.subscribe();
    upsert(&store, person("c1", "Anna", &["anna@example.com"]), 1);
    // Sync writes say that contacts changed.
    let set = changes.try_recv().unwrap();
    assert!(set.contacts);
    store
        .write_blocking(move |w| w.delete_account(account))
        .unwrap();
    let reader = store.reader().unwrap();
    let left: Vec<String> = reader
        .search_contacts("", 10)
        .unwrap()
        .into_iter()
        .map(|c| c.book)
        .collect();
    assert_eq!(left, ["collected"]);
    assert!(reader.server_contacts(BOOK).unwrap().is_empty());
    drop(reader);
    // Without its book, a person can't be stored.
    assert!(
        store
            .write_blocking(|w| w.upsert_server_contact(BOOK, &person("c2", "B", &[]), 1))
            .is_err()
    );
}

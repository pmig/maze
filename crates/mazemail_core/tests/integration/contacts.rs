//! The Address Book's reads over a seeded `contact` table: collected addresses, an
//! imported Thunderbird book and a Google book of the Gmail account (as contact sync
//! writes them), grouped, labeled, filtered and sorted.

use mazemail_core::{BookSource, ContactEdit, ContactError, ContactQuery, ContactSort};

use crate::common;

const VCARD: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Anna Example\r\nEMAIL:anna@example.com\r\n\
     EMAIL;TYPE=work:anna@work.example\r\nTEL;TYPE=cell:+43 660 1234567\r\nORG:Example GmbH\r\n\
     TITLE:Head of Research\r\nEND:VCARD\r\n";

/// Seeds the books and returns the Google book's id.
async fn seed(t: &common::TestCore) -> String {
    let fixture = common::fixture(&t.core).await;
    let gmail = t
        .core
        .accounts()
        .unwrap()
        .into_iter()
        .find(|a| a.id == fixture.gmail)
        .unwrap();
    let google = format!("google:{}", gmail.uuid);
    let google_other = format!("google-other:{}", gmail.uuid);
    let orphan = "google:00000000-0000-4000-8000-000000000000".to_owned();
    let books = (google.clone(), google_other, orphan);
    t.core
        .store()
        .write(move |w| {
            let (google, google_other, orphan) = books;
            // Written to: Anna twice (lowercase), Bob once, Dora never by name.
            w.bump_contact("anna@example.com", None, 1_000)?;
            w.bump_contact("anna@example.com", None, 3_000)?;
            w.bump_contact("bob@example.net", Some("Bob"), 2_000)?;
            w.bump_contact("dora@example.org", None, 500)?;
            // Imported from Thunderbird.
            w.upsert_contact(
                "Personal Address Book",
                "Carl@Example.com",
                Some("Carl Imported"),
                None,
            )?;
            w.upsert_contact(
                "Personal Address Book",
                "bob@example.net",
                Some("Bob Builder"),
                None,
            )?;
            // Synced from Google, Anna spelled in capitals.
            w.upsert_contact(
                &google,
                "Anna@Example.com",
                Some("Anna Example"),
                Some(VCARD),
            )?;
            w.upsert_contact(&google_other, "anna@example.com", Some("anna"), None)?;
            w.upsert_contact(&google, "erin@example.com", Some("Erin"), None)?;
            // A book of an account that is gone.
            w.upsert_contact(&orphan, "zed@example.com", Some("Zed"), None)?;
            Ok(())
        })
        .await
        .unwrap();
    google
}

#[tokio::test(flavor = "multi_thread")]
async fn books_come_per_account_then_collected_imported_and_other() {
    let t = common::open().await;
    assert!(t.core.address_books().unwrap().is_empty());
    let google = seed(&t).await;
    let books = t.core.address_books().unwrap();
    let names: Vec<(&str, u64)> = books.iter().map(|b| (b.name.as_str(), b.count)).collect();
    assert_eq!(
        names,
        [
            ("My Contacts", 2),
            ("Other Contacts", 1),
            ("Collected", 3),
            ("Personal Address Book", 2),
            ("My Contacts", 1),
        ]
    );
    assert_eq!(books[0].id, google);
    match &books[0].source {
        BookSource::Account {
            account_name,
            provider,
            kind,
            ..
        } => {
            assert_eq!(account_name, "Gmail");
            assert_eq!(provider, "google");
            assert_eq!(kind, &None);
        }
        other => panic!("expected an account book, got {other:?}"),
    }
    assert!(matches!(books[2].source, BookSource::Collected));
    assert!(matches!(books[3].source, BookSource::Imported));
    assert!(matches!(books[4].source, BookSource::Orphaned { .. }));
    assert!(books.iter().all(|b| !b.editable));
    assert!(!t.core.contact_book_editable(&google));
    t.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn all_contacts_has_one_row_per_address_with_a_label_per_account() {
    let t = common::open().await;
    let google = seed(&t).await;
    let all = t.core.contacts(&ContactQuery::default()).unwrap();
    let rows: Vec<(Option<&str>, &str, Vec<&str>)> = all
        .iter()
        .map(|c| {
            (
                c.name.as_deref(),
                c.email.as_str(),
                c.labels.iter().map(|l| l.text.as_str()).collect(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            // Google's name and spelling win over collected; both Google books are one
            // label (one account).
            (
                Some("Anna Example"),
                "Anna@Example.com",
                vec!["Gmail", "Collected"]
            ),
            // The imported name wins over the collected one.
            (
                Some("Bob Builder"),
                "bob@example.net",
                vec!["Personal Address Book", "Collected"]
            ),
            (
                Some("Carl Imported"),
                "Carl@Example.com",
                vec!["Personal Address Book"]
            ),
            (None, "dora@example.org", vec!["Collected"]),
            (Some("Erin"), "erin@example.com", vec!["Gmail"]),
            (Some("Zed"), "zed@example.com", vec!["My Contacts"]),
        ]
    );
    let anna = &all[0];
    assert_eq!(anna.frequency, 2);
    assert_eq!(anna.last_used, Some(3_000));
    assert!(anna.labels[0].account.is_some());
    assert!(anna.labels[1].collected);

    // Last used first; never written to last, by name.
    let recent = t
        .core
        .contacts(&ContactQuery {
            sort: ContactSort::LastUsed,
            ..ContactQuery::default()
        })
        .unwrap();
    let emails: Vec<&str> = recent.iter().map(|c| c.email.as_str()).collect();
    assert_eq!(
        emails,
        [
            "Anna@Example.com",
            "bob@example.net",
            "dora@example.org",
            "Carl@Example.com",
            "erin@example.com",
            "zed@example.com",
        ]
    );

    // One book; its own entry's name first, else the name another book knows.
    let book = t
        .core
        .contacts(&ContactQuery {
            book: Some("collected".into()),
            ..ContactQuery::default()
        })
        .unwrap();
    let names: Vec<Option<&str>> = book.iter().map(|c| c.name.as_deref()).collect();
    assert_eq!(names, [Some("Anna Example"), Some("Bob"), None]);
    let google_rows = t
        .core
        .contacts(&ContactQuery {
            book: Some(google),
            ..ContactQuery::default()
        })
        .unwrap();
    assert_eq!(google_rows.len(), 2);

    // The filter looks at every version's name and the address, ignoring case.
    let found = |text: &str| -> Vec<String> {
        t.core
            .contacts(&ContactQuery {
                text: text.into(),
                ..ContactQuery::default()
            })
            .unwrap()
            .into_iter()
            .map(|c| c.email.to_lowercase())
            .collect()
    };
    assert_eq!(found("BUILDER"), ["bob@example.net"]);
    assert_eq!(
        found("example.com"),
        [
            "anna@example.com",
            "carl@example.com",
            "erin@example.com",
            "zed@example.com"
        ]
    );
    assert!(found("nobody").is_empty());
    t.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_detail_lists_every_books_version_with_its_vcard_fields() {
    let t = common::open().await;
    seed(&t).await;
    let anna = t.core.contact("ANNA@example.com").unwrap().unwrap();
    assert_eq!(anna.name.as_deref(), Some("Anna Example"));
    assert_eq!(anna.email, "Anna@Example.com");
    assert_eq!(anna.emails, ["Anna@Example.com", "anna@work.example"]);
    assert_eq!(anna.phones.len(), 1);
    assert_eq!(anna.phones[0].number, "+43 660 1234567");
    assert_eq!(anna.organization.as_deref(), Some("Example GmbH"));
    assert_eq!(anna.title.as_deref(), Some("Head of Research"));
    let versions: Vec<(&str, Option<&str>)> = anna
        .versions
        .iter()
        .map(|v| (v.book.name.as_str(), v.name.as_deref()))
        .collect();
    assert_eq!(
        versions,
        [
            ("My Contacts", Some("Anna Example")),
            ("Other Contacts", Some("anna")),
            ("Collected", None),
        ]
    );
    assert_eq!(anna.versions[2].frequency, 2);
    assert!(t.core.contact("nobody@example.com").unwrap().is_none());

    // Nothing can be edited until contact sync writes to the server.
    let edit = ContactEdit {
        book: anna.versions[0].book.id.clone(),
        contact: Some(anna.versions[0].id),
        name: Some("Anna".into()),
        emails: anna.emails.clone(),
        phones: Vec::new(),
        organization: None,
        title: None,
    };
    assert!(matches!(
        t.core.save_contact(edit).await,
        Err(ContactError::NotEditable { .. })
    ));
    t.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn contact_changes_reach_the_event_stream() {
    let t = common::open().await;
    let mut events = t.core.events();
    t.core
        .store()
        .write(|w| w.bump_contact("new@example.com", None, 10).map(drop))
        .await
        .unwrap();
    let batch = common::wait_for(&mut events, |b| b.contacts).await;
    assert!(batch.contacts);
    t.core.shutdown().await.unwrap();
}

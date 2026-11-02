//! Domains (PLAN-contacts §3b): the list with counts, filters and sorting, a domain's
//! contacts, the "Show Mail" search, and favicons without a fetcher.

use mazemail_core::{
    DomainFilter, DomainInfo, DomainKind, DomainSort, DomainSortField, SearchOpts,
    domain_mail_query,
};
use pretty_assertions::assert_eq;

use crate::common::{self, Place, TestCore};

async fn corpus() -> TestCore {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let mut events = t.core.events();
    for (uid, from, to, date) in [
        (
            1,
            "Anna <anna@mail.eu.northwind.example>",
            "me@work.example, Bob <bob@northwind.example>",
            "Mon, 6 Oct 2025 10:00:00 +0000",
        ),
        (
            2,
            "Bob <bob@Northwind.example>",
            "me@work.example",
            "Tue, 7 Oct 2025 10:00:00 +0000",
        ),
        (
            3,
            "Carol <carol@gmail.com>",
            "me@work.example",
            "Wed, 8 Oct 2025 10:00:00 +0000",
        ),
        (
            4,
            "Me <me@work.example>",
            "news@xn--bcher-kva.de",
            "Thu, 9 Oct 2025 10:00:00 +0000",
        ),
    ] {
        common::ingest(
            &t.core,
            Place::new(f.work, f.inbox, uid),
            common::raw_message(&format!("m{uid}@test"), from, to, "Hello", date, "Text"),
        )
        .await;
    }
    common::wait_for(&mut events, |batch| batch.domains).await;
    t.core
        .store()
        .write(|w| {
            w.upsert_contact("google:1", "dora@fabrikam.example", Some("Dora"), None)?;
            w.bump_contact("Dora@Fabrikam.example", Some("D."), 5)?;
            w.bump_contact("anna@northwind.example", None, 5)?;
            Ok(())
        })
        .await
        .unwrap();
    t.core.commit_index().await.unwrap();
    t
}

fn summary(domains: &[DomainInfo]) -> Vec<(&str, &str, DomainKind, u64, u64, u64, u64)> {
    domains
        .iter()
        .map(|d| {
            (
                d.name.as_str(),
                d.display_name.as_str(),
                d.kind,
                d.contact_count,
                d.message_count,
                d.received_count,
                d.sent_count,
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn domains_with_counts_filters_and_sorting() {
    let t = corpus().await;
    let all = t
        .core
        .domains(&DomainFilter::default(), DomainSort::default())
        .unwrap();
    use DomainKind::{Organization, Personal};
    assert_eq!(
        summary(&all),
        vec![
            ("xn--bcher-kva.de", "bücher.de", Organization, 0, 1, 0, 1),
            (
                "fabrikam.example",
                "fabrikam.example",
                Organization,
                1,
                0,
                0,
                0
            ),
            ("gmail.com", "gmail.com", Personal, 0, 1, 1, 0),
            (
                "northwind.example",
                "northwind.example",
                Organization,
                1,
                2,
                2,
                0
            ),
        ]
    );
    // Your own domain isn't listed for your own address.
    assert!(all.iter().all(|d| d.name != "work.example"));
    let northwind = all.iter().find(|d| d.name == "northwind.example").unwrap();
    assert!(northwind.last_seen.is_some());
    assert!(!northwind.has_favicon);
    let fabrikam = all.iter().find(|d| d.name == "fabrikam.example").unwrap();
    assert_eq!(fabrikam.last_seen, None);

    let organizations = t
        .core
        .domains(
            &DomainFilter {
                kind: Some(Organization),
                ..DomainFilter::default()
            },
            DomainSort {
                field: DomainSortField::Messages,
                descending: true,
            },
        )
        .unwrap();
    let names: Vec<&str> = organizations.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        ["northwind.example", "xn--bcher-kva.de", "fabrikam.example"]
    );

    let by_text = |text: &str| {
        t.core
            .domains(
                &DomainFilter {
                    text: Some(text.to_owned()),
                    ..DomainFilter::default()
                },
                DomainSort::default(),
            )
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(by_text("BÜCHER"), ["xn--bcher-kva.de"]);
    assert_eq!(by_text("xn--"), ["xn--bcher-kva.de"]);
    assert_eq!(by_text("wind"), ["northwind.example"]);
    assert!(by_text("nothing").is_empty());

    let newest_first = t
        .core
        .domains(
            &DomainFilter::default(),
            DomainSort {
                field: DomainSortField::LastSeen,
                descending: true,
            },
        )
        .unwrap();
    let names: Vec<&str> = newest_first.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "xn--bcher-kva.de",
            "gmail.com",
            "northwind.example",
            "fabrikam.example"
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn contacts_mail_and_favicons_of_a_domain() {
    let t = corpus().await;
    let contacts = t.core.domain_contacts("Mail.Fabrikam.example").unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0].email, "dora@fabrikam.example");
    // The address book's name wins over the collected one.
    assert_eq!(contacts[0].display_name.as_deref(), Some("Dora"));
    assert_eq!(contacts[0].books, ["google:1", "collected"]);
    assert!(t.core.domain_contacts("192.0.2.1").unwrap().is_empty());

    let query = domain_mail_query("northwind.example");
    assert_eq!(query, "from:@northwind.example");
    let found = t.core.search(&query, SearchOpts::default()).await.unwrap();
    assert_eq!(found.total_messages, 2);
    let none = t
        .core
        .search(
            &domain_mail_query("fabrikam.example"),
            SearchOpts::default(),
        )
        .await
        .unwrap();
    assert_eq!(none.total_messages, 0);

    // Without a fetcher (tests, `CoreConfig::favicons` unset) nothing is fetched.
    assert_eq!(t.core.domain_favicon("northwind.example").unwrap(), None);
    assert_eq!(t.core.domain_favicon("not a domain").unwrap(), None);
}

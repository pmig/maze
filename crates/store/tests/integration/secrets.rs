//! Account secrets in the encrypted database (ADR-017).

use pretty_assertions::assert_eq;
use store::{AccountKind, NewAccount, Queries};

use crate::common;

fn secret(store: &store::Store, account: store::Uuid, kind: &str) -> Option<String> {
    store
        .reader()
        .unwrap()
        .account_secret(account, kind)
        .unwrap()
        .map(|secret| secret.as_str().to_owned())
}

#[test]
fn secrets_round_trip_per_account_and_go_with_the_account() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let work = common::fixture(&store, AccountKind::Imap).account;
    let home = store
        .write_blocking(|w| {
            w.create_account(&NewAccount {
                kind: AccountKind::Imap,
                name: "Home".to_owned(),
                config_json: "{}".to_owned(),
                ordinal: 1,
            })
        })
        .unwrap();
    let reader = store.reader().unwrap();
    let work_uuid = reader.account(work).unwrap().unwrap().uuid;
    let home_uuid = reader.account(home).unwrap().unwrap().uuid;
    drop(reader);
    assert_eq!(secret(&store, work_uuid, "password"), None);

    store
        .write_blocking(move |w| {
            w.set_account_secret(work_uuid, "password", "first")?;
            w.set_account_secret(work_uuid, "password", "second")?;
            w.set_account_secret(work_uuid, "smtp-password", "smtp")?;
            w.set_account_secret(home_uuid, "password", "home")
        })
        .unwrap();
    assert_eq!(
        secret(&store, work_uuid, "password").as_deref(),
        Some("second")
    );
    assert_eq!(
        secret(&store, work_uuid, "smtp-password").as_deref(),
        Some("smtp")
    );
    assert_eq!(
        secret(&store, home_uuid, "password").as_deref(),
        Some("home")
    );
    assert_eq!(secret(&store, home_uuid, "smtp-password"), None);

    // Deleting is idempotent and per kind.
    store
        .write_blocking(move |w| {
            w.delete_account_secret(work_uuid, "smtp-password")?;
            w.delete_account_secret(work_uuid, "smtp-password")
        })
        .unwrap();
    assert_eq!(secret(&store, work_uuid, "smtp-password"), None);
    assert_eq!(
        secret(&store, work_uuid, "password").as_deref(),
        Some("second")
    );

    // No secret for an account that doesn't exist.
    let unknown = store::Uuid::from_u128(42);
    let err = store
        .write_blocking(move |w| w.set_account_secret(unknown, "password", "canary-secret"))
        .unwrap_err();
    assert!(matches!(err, store::Error::Invalid(_)), "{err:?}");
    assert!(!format!("{err} {err:?}").contains("canary-secret"));

    // Removing the account removes its secrets, and nothing else.
    store
        .write_blocking(move |w| w.delete_account(work))
        .unwrap();
    assert_eq!(secret(&store, work_uuid, "password"), None);
    assert_eq!(
        secret(&store, home_uuid, "password").as_deref(),
        Some("home")
    );

    // New accounts never had keychain items.
    assert_eq!(
        store.reader().unwrap().legacy_secret_accounts().unwrap(),
        Vec::<store::Uuid>::new()
    );
    common::close(store);
}

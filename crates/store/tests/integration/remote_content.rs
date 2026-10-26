//! Senders allowed to load remote content (schema v11).

use store::Queries as _;

use crate::common::{close, open};

#[test]
fn senders_can_be_allowed_and_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let allowed = |store: &store::Store, email: &str| {
        store
            .reader()
            .unwrap()
            .remote_content_allowed(email)
            .unwrap()
    };
    assert!(!allowed(&store, "news@shop.example"));
    store
        .write_blocking(|w| w.allow_remote_content(" News@Shop.example ", 1))
        .unwrap();
    // Idempotent.
    store
        .write_blocking(|w| w.allow_remote_content("news@shop.example", 2))
        .unwrap();
    assert!(allowed(&store, "NEWS@shop.example"));
    assert!(!allowed(&store, "other@shop.example"));
    assert!(!allowed(&store, ""));
    assert!(
        store
            .write_blocking(|w| w.allow_remote_content("not an address", 3))
            .is_err()
    );
    close(store);

    // It is kept (in the encrypted database).
    let store = open(dir.path());
    assert!(allowed(&store, "news@shop.example"));
    store
        .write_blocking(|w| w.forget_remote_content("news@shop.example"))
        .unwrap();
    assert!(!allowed(&store, "news@shop.example"));
    close(store);
}

//! The personal spelling dictionary (schema v12).

use store::Queries as _;

use crate::common::{close, open};

#[test]
fn spelling_words_are_added_listed_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let words = |store: &store::Store| store.reader().unwrap().spelling_words().unwrap();
    assert!(words(&store).is_empty());
    store
        .write_blocking(|w| w.add_spelling_word(" Glasskube ", 1))
        .unwrap();
    store
        .write_blocking(|w| w.add_spelling_word("mazemail", 2))
        .unwrap();
    // Idempotent; case is kept (the spell checker decides what it matches).
    store
        .write_blocking(|w| w.add_spelling_word("Glasskube", 3))
        .unwrap();
    store
        .write_blocking(|w| w.add_spelling_word("Jänner", 4))
        .unwrap();
    assert_eq!(words(&store), ["Glasskube", "Jänner", "mazemail"]);
    for bad in ["", "  ", "two words"] {
        assert!(
            store
                .write_blocking(|w| w.add_spelling_word(bad, 5))
                .is_err(),
            "{bad:?}"
        );
    }
    close(store);

    // Kept (in the encrypted database).
    let store = open(dir.path());
    assert_eq!(words(&store), ["Glasskube", "Jänner", "mazemail"]);
    assert!(
        store
            .write_blocking(|w| w.remove_spelling_word("mazemail"))
            .unwrap()
    );
    assert!(
        !store
            .write_blocking(|w| w.remove_spelling_word("mazemail"))
            .unwrap()
    );
    assert_eq!(words(&store), ["Glasskube", "Jänner"]);
    close(store);
}

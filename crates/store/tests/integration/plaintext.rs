//! Nothing under the data dir contains message content or account secrets in plaintext
//! (PLAN §4.17, ADR-017).

use std::fs;
use std::path::{Path, PathBuf};

use mazemail_types::{Address, Flags};
use store::{
    AccountKind, BodyState, IdentityInput, NewLocation, NewMessage, Queries, SignatureInput,
    ThreadAssignment,
};

use crate::common;

const SUBJECT: &str = "ZebraQuokka quarterly canary subject";
const ADDRESS: &str = "zebra.quokka@plaintext-canary.example";
const NAME: &str = "Zebrina Quokkason";
const BODY: &str = "QuokkaBodyCanary: the merger closes on Friday";
const SNIPPET: &str = "QuokkaSnippetCanary preview text";
const MESSAGE_ID: &str = "quokka-canary-id@plaintext-canary.example";
const SIGNATURE: &str = "QuokkaSignatureCanary";
const KEYWORD: &str = "quokkakeywordcanary";
const PASSWORD: &str = "QuokkaPasswordCanary-hunter2";
const REFRESH_TOKEN: &str = "1//QuokkaRefreshCanary";

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn hits(dir: &Path) -> Vec<String> {
    let needles = [
        SUBJECT,
        ADDRESS,
        NAME,
        BODY,
        SNIPPET,
        MESSAGE_ID,
        SIGNATURE,
        KEYWORD,
        PASSWORD,
        REFRESH_TOKEN,
    ];
    // Also short, distinctive fragments, in case something stores pieces.
    let fragments = [
        "ZebraQuokka",
        "plaintext-canary",
        "QuokkaBody",
        "Zebrina",
        "QuokkaPassword",
        "QuokkaRefresh",
    ];
    let mut found = Vec::new();
    let mut all = Vec::new();
    files(dir, &mut all);
    assert!(!all.is_empty());
    for path in all {
        let bytes = fs::read(&path).unwrap();
        for needle in needles.iter().chain(fragments.iter()) {
            let needle = needle.as_bytes();
            if bytes.windows(needle.len()).any(|w| w == needle) {
                found.push(format!(
                    "{} contains {:?}",
                    path.display(),
                    String::from_utf8_lossy(needle)
                ));
            }
        }
    }
    found
}

#[test]
fn no_plaintext_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);

    let raw = format!(
        "From: {NAME} <{ADDRESS}>\r\nTo: {ADDRESS}\r\nSubject: {SUBJECT}\r\nMessage-ID: <{MESSAGE_ID}>\r\n\r\n{}",
        BODY.repeat(200)
    );
    let mut synthetic = Vec::new();
    for i in 0..50u32 {
        let mut m = NewMessage::new(f.account, 1_700_000_000 + i64::from(i));
        m.rfc_message_id = Some(format!("{i}-{MESSAGE_ID}"));
        m.in_reply_to = Some(MESSAGE_ID.to_owned());
        m.references = vec![MESSAGE_ID.to_owned()];
        m.subject = Some(format!("{SUBJECT} #{i}"));
        m.subject_key = Some(SUBJECT.to_lowercase());
        m.from = Some(Address::new(Some(NAME), ADDRESS));
        m.to = vec![Address::new(Some(NAME), ADDRESS)];
        m.delivered_to = Some(ADDRESS.to_owned());
        m.snippet = Some(SNIPPET.to_owned());
        m.thread = ThreadAssignment::Resolve { by_subject: true };
        let mut location = NewLocation::new(f.inbox, Some(i + 1), Flags::empty());
        location.keywords = vec![KEYWORD.to_owned()];
        m.locations = vec![location];
        synthetic.push(m);
    }
    let staged = store.blobs().put(raw.as_bytes()).unwrap();
    let blob = staged.id();
    store
        .write_blocking(move |w| {
            for m in &synthetic {
                let out = w.upsert_message(m)?;
                w.set_message_body(out.message, &staged, BodyState::Full)?;
            }
            w.create_tag(KEYWORD, "Quokka", None, 9)?;
            let signature = w.create_signature(&SignatureInput {
                name: SIGNATURE.to_owned(),
                body_html: Some(format!("<p>{SIGNATURE}</p>")),
                body_text: Some(SIGNATURE.to_owned()),
                ordinal: 0,
            })?;
            w.create_identity(
                f.account,
                &IdentityInput {
                    email: ADDRESS.to_owned(),
                    display_name: Some(NAME.to_owned()),
                    signature: Some(signature),
                    ..IdentityInput::default()
                },
            )?;
            w.bump_contact(ADDRESS, Some(NAME), 1)?;
            w.audit("user", "send", Some(&format!(r#"{{"to":"{ADDRESS}"}}"#)))?;
            let uuid = w
                .account(f.account)?
                .ok_or(store::Error::Invalid("no account"))?
                .uuid;
            w.set_account_secret(uuid, "password", PASSWORD)?;
            w.set_account_secret(uuid, "oauth-refresh", REFRESH_TOKEN)?;
            Ok(())
        })
        .unwrap();
    store.sendlog().record(MESSAGE_ID, None, None).unwrap();
    store
        .blobs()
        .cache_put(&blob, "reader", 1, format!("<p>{BODY}</p>").as_bytes())
        .unwrap();
    // The data is really there.
    let reader = store.reader().unwrap();
    let keys = reader
        .message_ids_in_scope(&store::MessageScope::Folder(f.inbox))
        .unwrap();
    assert_eq!(keys.len(), 50);
    assert_eq!(
        reader.message(keys[0]).unwrap().unwrap().subject.as_deref(),
        Some(format!("{SUBJECT} #0").as_str())
    );
    let uuid = reader.account(f.account).unwrap().unwrap().uuid;
    assert_eq!(
        reader
            .account_secret(uuid, "password")
            .unwrap()
            .unwrap()
            .as_str(),
        PASSWORD
    );
    drop(reader);
    assert_eq!(store.blobs().read(&blob).unwrap(), raw.as_bytes());

    // While open (WAL and shm files included) …
    let open_hits = hits(tmp.path());
    assert!(open_hits.is_empty(), "{open_hits:#?}");

    // … and after closing.
    common::close(store);
    let closed_hits = hits(tmp.path());
    assert!(closed_hits.is_empty(), "{closed_hits:#?}");

    // Control: the scan does find plaintext when there is some.
    fs::write(tmp.path().join("mail/tmp/control"), SUBJECT).unwrap();
    assert_eq!(hits(tmp.path()).len(), 2);
}

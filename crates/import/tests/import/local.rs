//! Generic mbox/Maildir import, idempotency and export round trips.

use std::fs;
use std::path::Path;

use import::mbox::{Envelope, MboxWriter};
use import::{
    ExportFormat, ExportScope, ImportTarget, MozillaState, NoProgress, PLAINTEXT_WARNING, export,
    import_maildir, import_mbox, maildir::MaildirWriter,
};
use mazemail_types::{Flags, FolderId};
use pretty_assertions::assert_eq;
use store::{AccountKind, MessageScope, Queries, Store};

use crate::common::Env;

/// A message with CRLF line endings, as stored.
fn message(i: usize, extra_body: &str) -> Vec<u8> {
    format!(
        "Date: Mon, 0{} Jan 2024 10:00:00 +0000\r\n\
         From: Sender {i} <sender{i}@example.com>\r\n\
         To: me@example.org\r\n\
         Subject: Message {i}\r\n\
         Message-ID: <m{i}@example.com>\r\n\
         \r\n\
         Body {i}\r\n\
         From the start of a line\r\n\
         >From an escaped line\r\n\
         {extra_body}",
        i % 9 + 1
    )
    .into_bytes()
}

/// Writes an mbox with Thunderbird status headers, the way Thunderbird would.
fn write_mbox(path: &Path, messages: &[(Vec<u8>, Flags, Vec<String>)]) {
    let mut writer = MboxWriter::new(Vec::new());
    for (i, (raw, flags, keywords)) in messages.iter().enumerate() {
        let state = MozillaState::from_flags(*flags, keywords);
        writer
            .write_message(
                &Envelope {
                    sender: None,
                    date: Some(1_704_000_000 + i as i64),
                },
                Some(&state),
                raw,
            )
            .unwrap();
    }
    fs::write(path, writer.finish().unwrap()).unwrap();
}

/// `(blob bytes, flags, keywords)` of every message in a folder, by Message-ID.
fn folder_contents(store: &Store, folder: FolderId) -> Vec<(String, Vec<u8>, Flags, Vec<String>)> {
    let reader = store.reader().unwrap();
    let mut out: Vec<_> = reader
        .message_ids_in_scope(&MessageScope::Folder(folder))
        .unwrap()
        .into_iter()
        .map(|id| {
            let m = reader.message(id).unwrap().unwrap();
            let location = m.locations.iter().find(|l| l.folder == folder).unwrap();
            (
                m.rfc_message_id.clone().unwrap_or_default(),
                store.blobs().read(&m.blob.unwrap()).unwrap(),
                location.flags,
                location.keywords.clone(),
            )
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

fn folder_by_name(store: &Store, name: &str) -> FolderId {
    store
        .reader()
        .unwrap()
        .folders(None)
        .unwrap()
        .into_iter()
        .find(|f| f.display_name == name)
        .unwrap()
        .id
}

#[test]
fn mbox_import_is_idempotent_and_keeps_flags() {
    let env = Env::new();
    let path = env.scratch("src").join("Archive 2023.mbox");
    let messages = vec![
        (message(1, ""), Flags::SEEN, vec!["$label1".to_owned()]),
        (
            message(2, "more\r\n"),
            Flags::FLAGGED | Flags::ANSWERED,
            vec![],
        ),
        (message(3, ""), Flags::empty(), vec!["Todo".to_owned()]),
        // An identical copy (same bytes) in the same file.
        (message(1, ""), Flags::SEEN, vec![]),
    ];
    write_mbox(&path, &messages);

    let first = import_mbox(&env.ingestor, &path, &ImportTarget::default(), &NoProgress).unwrap();
    assert_eq!(first.messages, 4);
    assert_eq!(first.imported, 3);
    assert_eq!(first.duplicates, 1);
    assert_eq!(first.folders_created, 1);
    assert_eq!(first.failed, 0);

    let reader = env.store.reader().unwrap();
    let accounts = reader.accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].kind, AccountKind::Local);
    assert_eq!(accounts[0].name, "Local Folders");
    drop(reader);
    let folder = folder_by_name(&env.store, "Archive 2023");
    let contents = folder_contents(&env.store, folder);
    assert_eq!(contents.len(), 3);
    assert_eq!(contents[0].0, "m1@example.com");
    // Stored bytes are the message without Thunderbird's headers, unescaped.
    assert_eq!(contents[0].1, message(1, ""));
    assert_eq!(contents[0].2, Flags::SEEN);
    assert_eq!(contents[0].3, vec!["$label1"]);
    assert_eq!(contents[1].2, Flags::FLAGGED | Flags::ANSWERED);
    assert_eq!(contents[2].3, vec!["todo"]);

    let second = import_mbox(&env.ingestor, &path, &ImportTarget::default(), &NoProgress).unwrap();
    assert_eq!(second.imported, 0);
    assert_eq!(second.duplicates, 4);
    assert_eq!(second.folders_created, 0);
    assert_eq!(folder_contents(&env.store, folder).len(), 3);

    // Into another folder it's a separate copy.
    let target = ImportTarget {
        account: None,
        folder: Some(vec!["Other".to_owned(), "Nested".to_owned()]),
    };
    let third = import_mbox(&env.ingestor, &path, &target, &NoProgress).unwrap();
    assert_eq!(third.imported, 3);
    assert_eq!(third.folders_created, 2);
    env.close();
}

#[test]
fn export_then_import_gives_identical_messages() {
    for format in [ExportFormat::Mbox, ExportFormat::Maildir] {
        let env = Env::new();
        let path = env.scratch("src").join("Inbox");
        let messages = vec![
            (
                message(1, ""),
                Flags::SEEN | Flags::FORWARDED,
                vec!["$label2".to_owned()],
            ),
            (
                message(2, "\r\nends with a blank line\r\n\r\n"),
                Flags::DELETED,
                vec![],
            ),
            (
                message(3, ">>>From deep\r\n"),
                Flags::DRAFT | Flags::JUNK,
                vec!["a".to_owned(), "b".to_owned()],
            ),
            // No Message-ID: dedupe by content.
            (
                b"Subject: no id\r\nFrom: x@example.com\r\n\r\nplain\r\n".to_vec(),
                Flags::empty(),
                vec![],
            ),
        ];
        write_mbox(&path, &messages);
        import_mbox(&env.ingestor, &path, &ImportTarget::default(), &NoProgress).unwrap();
        let inbox = folder_by_name(&env.store, "Inbox");
        let before = folder_contents(&env.store, inbox);
        assert_eq!(before.len(), 4);

        let out = env.scratch("export");
        let report = export(
            &env.store,
            &ExportScope {
                account: None,
                folder: Some(inbox),
            },
            format,
            &out,
            &NoProgress,
        )
        .unwrap();
        assert_eq!(report.messages, 4);
        assert_eq!(report.folders, 1);
        assert_eq!(report.warning, PLAINTEXT_WARNING);
        let exported = out.join("Inbox");
        assert!(exported.exists());

        let target = ImportTarget {
            account: None,
            folder: Some(vec!["Reimported".to_owned()]),
        };
        let stats = match format {
            ExportFormat::Mbox => import_mbox(&env.ingestor, &exported, &target, &NoProgress),
            ExportFormat::Maildir => import_maildir(&env.ingestor, &exported, &target, &NoProgress),
        }
        .unwrap();
        assert_eq!(stats.imported, 4, "{format:?}");
        let after = folder_contents(&env.store, folder_by_name(&env.store, "Reimported"));
        // Maildir has no letters for the junk flags; mbox carries everything.
        let expected: Vec<_> = before
            .iter()
            .map(|(id, raw, flags, kw)| {
                let flags = match format {
                    ExportFormat::Mbox => *flags,
                    ExportFormat::Maildir => *flags - Flags::JUNK - Flags::NOT_JUNK,
                };
                (id.clone(), raw.clone(), flags, kw.clone())
            })
            .collect();
        assert_eq!(after, expected, "{format:?}");

        // Exporting again into the same place refuses to overwrite.
        assert!(
            export(
                &env.store,
                &ExportScope {
                    account: None,
                    folder: Some(inbox),
                },
                format,
                &out,
                &NoProgress,
            )
            .is_err()
        );
        env.close();
    }
}

#[test]
fn exports_whole_accounts_in_thunderbird_layout() {
    let env = Env::new();
    let path = env.scratch("src").join("x");
    write_mbox(&path, &[(message(1, ""), Flags::SEEN, vec![])]);
    for folder in [
        vec!["Inbox".to_owned()],
        vec!["Inbox".to_owned(), "Sub/One".to_owned()],
    ] {
        let target = ImportTarget {
            account: None,
            folder: Some(folder),
        };
        import_mbox(&env.ingestor, &path, &target, &NoProgress).unwrap();
    }
    let out = env.scratch("all");
    let report = export(
        &env.store,
        &ExportScope::default(),
        ExportFormat::Mbox,
        &out,
        &NoProgress,
    )
    .unwrap();
    assert_eq!(report.folders, 2);
    assert_eq!(report.messages, 2);
    assert!(out.join("Local Folders").join("Inbox").is_file());
    assert!(
        out.join("Local Folders")
            .join("Inbox.sbd")
            .join("Sub_One")
            .is_file()
    );
    env.close();
}

#[test]
fn maildir_import_with_subfolders_and_keywords() {
    let env = Env::new();
    let root = env.scratch("maildir").join("Mail");
    let mut writer = MaildirWriter::create(&root).unwrap();
    writer
        .write(
            &message(1, ""),
            Flags::SEEN | Flags::FLAGGED,
            &["Projekt".to_owned()],
        )
        .unwrap();
    writer.write(&message(2, ""), Flags::empty(), &[]).unwrap();
    writer.finish().unwrap();
    let mut sent = MaildirWriter::create(&root.join(".Sent.2024")).unwrap();
    sent.write(&message(3, ""), Flags::SEEN, &[]).unwrap();
    sent.finish().unwrap();
    fs::write(root.join("new").join("999.new.host"), message(4, "")).unwrap();

    let stats =
        import_maildir(&env.ingestor, &root, &ImportTarget::default(), &NoProgress).unwrap();
    assert_eq!(stats.imported, 4);
    assert_eq!(stats.folders_created, 3);
    let mail = folder_contents(&env.store, folder_by_name(&env.store, "Mail"));
    assert_eq!(mail.len(), 3);
    assert_eq!(mail[0].2, Flags::SEEN | Flags::FLAGGED);
    assert_eq!(mail[0].3, vec!["projekt"]);
    assert_eq!(mail[2].2, Flags::empty());
    let sent = folder_contents(&env.store, folder_by_name(&env.store, "2024"));
    assert_eq!(sent.len(), 1);

    let again =
        import_maildir(&env.ingestor, &root, &ImportTarget::default(), &NoProgress).unwrap();
    assert_eq!(again.imported, 0);
    assert_eq!(again.duplicates, 4);
    assert!(
        import_maildir(
            &env.ingestor,
            &env.scratch("empty"),
            &ImportTarget::default(),
            &NoProgress
        )
        .is_err()
    );
    env.close();
}

#[test]
fn truncated_and_garbage_mbox_files_import_what_they_can() {
    let env = Env::new();
    let path = env.scratch("src").join("broken");
    let mut writer = MboxWriter::new(Vec::new());
    for i in 1..=3 {
        writer
            .write_message(&Envelope::default(), None, &message(i, "tail line\r\n"))
            .unwrap();
    }
    let mut bytes = b"leading garbage\r\n".to_vec();
    bytes.extend(writer.finish().unwrap());
    bytes.truncate(bytes.len() - 20);
    fs::write(&path, &bytes).unwrap();
    let stats = import_mbox(&env.ingestor, &path, &ImportTarget::default(), &NoProgress).unwrap();
    assert_eq!(stats.messages, 3);
    assert_eq!(stats.imported, 3);
    assert_eq!(stats.skipped_bytes, 17);
    assert_eq!(stats.bytes, bytes.len() as u64);

    // Not a local account.
    let imap = env
        .store
        .write_blocking(|w| {
            w.create_account(&store::NewAccount {
                kind: AccountKind::Imap,
                name: "x".to_owned(),
                config_json: "{}".to_owned(),
                ordinal: 9,
            })
        })
        .unwrap();
    let target = ImportTarget {
        account: Some(imap),
        folder: None,
    };
    assert!(import_mbox(&env.ingestor, &path, &target, &NoProgress).is_err());
    env.close();
}

//! mbox/Maildir uploads into IMAP folders (queued `append` ops), their duplicate
//! check, and export round trips.

use std::fs;
use std::path::Path;

use import::mbox::{Envelope, MboxWriter};
use import::{
    ExportFormat, ExportScope, MozillaState, NoProgress, PLAINTEXT_WARNING, UploadTarget, export,
    maildir::MaildirWriter, upload_maildir, upload_mbox,
};
use ingest::{MessageMeta, RawItem};
use mazemail_types::{AccountId, Flags, FolderId};
use pretty_assertions::assert_eq;
use store::{
    AccountKind, GcOptions, MessageScope, NewAccount, NewFolder, NewLocation, Queries, Store,
};
use sync::ops::OpPayload;

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

fn account(store: &Store, kind: AccountKind, name: &str) -> AccountId {
    let name = name.to_owned();
    store
        .write_blocking(move |w| {
            w.create_account(&NewAccount {
                kind,
                name,
                config_json: "{}".to_owned(),
                ordinal: 0,
            })
        })
        .unwrap()
}

/// A folder of `account`; `remote` is its server name (`None`: not on the server).
fn folder(
    store: &Store,
    account: AccountId,
    parent: Option<FolderId>,
    name: &str,
    remote: Option<&str>,
) -> FolderId {
    let (name, remote) = (name.to_owned(), remote.map(str::to_owned));
    store
        .write_blocking(move |w| {
            w.create_folder(&NewFolder {
                account,
                parent,
                remote_name: remote,
                display_name: name,
                role: None,
                is_label: false,
                sync_policy: None,
                ordinal: 0,
            })
        })
        .unwrap()
}

/// An IMAP account with a server folder.
fn imap_target(env: &Env, name: &str) -> UploadTarget {
    let account = account(&env.store, AccountKind::Imap, "Work");
    let folder = folder(&env.store, account, None, name, Some(name));
    UploadTarget { account, folder }
}

/// `(message bytes, flags, keywords)` of the uploads queued for `target`, by bytes.
fn queued(store: &Store, target: UploadTarget) -> Vec<(Vec<u8>, Flags, Vec<String>)> {
    let reader = store.reader().unwrap();
    let mut out: Vec<_> = reader
        .pending_ops(target.account)
        .unwrap()
        .iter()
        .filter_map(|op| match OpPayload::from_pending(op).unwrap() {
            OpPayload::Append {
                folder,
                blob,
                flags,
                keywords,
            } if folder == target.folder => {
                Some((store.blobs().read(&blob).unwrap(), flags, keywords))
            }
            _ => None,
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Stores messages in a folder the way the sync engine does (with UIDs).
fn store_synced(env: &Env, target: UploadTarget, messages: &[(Vec<u8>, Flags, Vec<String>)]) {
    let items = messages
        .iter()
        .enumerate()
        .map(|(n, (raw, flags, keywords))| RawItem {
            meta: MessageMeta::new(
                target.account,
                NewLocation {
                    folder: target.folder,
                    uid: Some(n as u32 + 1),
                    modseq: None,
                    flags: *flags,
                    keywords: keywords.clone(),
                },
            ),
            raw: raw.clone(),
        })
        .collect();
    for result in env.ingestor.ingest_raw_batch_blocking(items) {
        result.unwrap();
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn mbox_upload_queues_appends_with_flags_and_skips_duplicates() {
    let env = Env::new();
    let target = imap_target(&env, "Archive 2023");
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

    let first = upload_mbox(&env.store, &path, target, "cli", &NoProgress).unwrap();
    assert_eq!(first.messages, 4);
    assert_eq!(first.queued, 3);
    assert_eq!(first.duplicates, 1);
    assert_eq!(first.failed, 0);
    // Nothing is stored locally: the folder's next sync brings the messages.
    let reader = env.store.reader().unwrap();
    assert!(
        reader
            .message_ids_in_scope(&MessageScope::Folder(target.folder))
            .unwrap()
            .is_empty()
    );
    let ops = reader.pending_ops(target.account).unwrap();
    assert!(
        ops.iter()
            .all(|op| op.kind == "append" && op.origin == "cli")
    );
    drop(reader);
    let uploads = queued(&env.store, target);
    assert_eq!(uploads.len(), 3);
    // The server gets the message without Thunderbird's headers, unescaped, with its
    // flags and tags.
    assert_eq!(
        uploads[0],
        (message(1, ""), Flags::SEEN, vec!["$label1".to_owned()])
    );
    assert_eq!(
        uploads[1],
        (
            message(2, "more\r\n"),
            Flags::FLAGGED | Flags::ANSWERED,
            vec![]
        )
    );
    assert_eq!(uploads[2].2, vec!["todo"]);

    // The queued ops keep their blobs through garbage collection.
    block_on(env.store.collect_garbage(GcOptions::full())).unwrap();
    assert_eq!(queued(&env.store, target).len(), 3);

    // Again while the uploads are queued: nothing new.
    let second = upload_mbox(&env.store, &path, target, "cli", &NoProgress).unwrap();
    assert_eq!(second.queued, 0);
    assert_eq!(second.duplicates, 4);

    // Once uploaded and synced (the server's copy may differ in bytes), the Message-ID
    // still recognizes them.
    let reader = env.store.reader().unwrap();
    let ops = reader.pending_ops(target.account).unwrap();
    drop(reader);
    env.store
        .write_blocking(move |w| {
            for op in &ops {
                w.op_done(op.id)?;
            }
            Ok(())
        })
        .unwrap();
    let synced: Vec<_> = (1..=3)
        .map(|i| (message(i, "added by the server\r\n"), Flags::SEEN, vec![]))
        .collect();
    store_synced(&env, target, &synced);
    let third = upload_mbox(&env.store, &path, target, "cli", &NoProgress).unwrap();
    assert_eq!(third.queued, 0);
    assert_eq!(third.duplicates, 4);

    // Into another folder it's a separate copy.
    let other = UploadTarget {
        account: target.account,
        folder: folder(&env.store, target.account, None, "Other", Some("Other")),
    };
    let fourth = upload_mbox(&env.store, &path, other, "cli", &NoProgress).unwrap();
    assert_eq!(fourth.queued, 3);
    env.close();
}

#[test]
fn messages_without_message_id_are_recognized_by_date_sender_and_subject() {
    let env = Env::new();
    let target = imap_target(&env, "INBOX");
    let path = env.scratch("src").join("no-ids");
    let raw = b"Date: Tue, 02 Jan 2024 10:00:00 +0000\r\nSubject: no id\r\n\
                From: x@example.com\r\n\r\nplain\r\n"
        .to_vec();
    write_mbox(&path, &[(raw.clone(), Flags::empty(), vec![])]);
    // The server added a Message-ID when it stored the message.
    let mut server_copy = b"Message-ID: <added@server.example>\r\n".to_vec();
    server_copy.extend_from_slice(&raw);
    store_synced(&env, target, &[(server_copy, Flags::empty(), vec![])]);
    let stats = upload_mbox(&env.store, &path, target, "cli", &NoProgress).unwrap();
    assert_eq!((stats.queued, stats.duplicates), (0, 1));
    env.close();
}

#[test]
fn only_server_folders_of_imap_accounts_are_targets() {
    let env = Env::new();
    let path = env.scratch("src").join("x");
    write_mbox(&path, &[(message(1, ""), Flags::SEEN, vec![])]);
    let imap = imap_target(&env, "INBOX");
    let not_on_server = UploadTarget {
        account: imap.account,
        folder: folder(&env.store, imap.account, None, "Nowhere", None),
    };
    let jmap = account(&env.store, AccountKind::Jmap, "JMAP");
    let wrong_account = UploadTarget {
        account: jmap,
        folder: imap.folder,
    };
    let jmap_folder = UploadTarget {
        account: jmap,
        folder: folder(&env.store, jmap, None, "Inbox", Some("Inbox")),
    };
    for target in [not_on_server, wrong_account, jmap_folder] {
        assert!(
            matches!(
                upload_mbox(&env.store, &path, target, "cli", &NoProgress),
                Err(import::Error::InvalidTarget(_))
            ),
            "{target:?}"
        );
    }
    // Nothing creates local accounts any more.
    assert!(
        env.store
            .write_blocking(|w| {
                w.create_account(&NewAccount {
                    kind: AccountKind::LegacyLocal,
                    name: "Local Folders".to_owned(),
                    config_json: "{}".to_owned(),
                    ordinal: 0,
                })
            })
            .is_err()
    );
    env.close();
}

#[test]
fn export_then_upload_gives_identical_messages() {
    for format in [ExportFormat::Mbox, ExportFormat::Maildir] {
        let env = Env::new();
        let inbox = imap_target(&env, "Inbox");
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
            (
                b"Subject: no id\r\nFrom: x@example.com\r\n\r\nplain\r\n".to_vec(),
                Flags::empty(),
                vec![],
            ),
        ];
        store_synced(&env, inbox, &messages);

        let out = env.scratch("export");
        let report = export(
            &env.store,
            &ExportScope {
                account: None,
                folder: Some(inbox.folder),
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

        let target = UploadTarget {
            account: inbox.account,
            folder: folder(&env.store, inbox.account, None, "Again", Some("Again")),
        };
        let stats = match format {
            ExportFormat::Mbox => upload_mbox(&env.store, &exported, target, "cli", &NoProgress),
            ExportFormat::Maildir => {
                upload_maildir(&env.store, &exported, target, "cli", &NoProgress)
            }
        }
        .unwrap();
        assert_eq!(stats.queued, 4, "{format:?}");
        // Maildir has no letters for the junk flags; mbox carries everything.
        let mut expected: Vec<_> = messages
            .iter()
            .map(|(raw, flags, kw)| {
                let flags = match format {
                    ExportFormat::Mbox => *flags,
                    ExportFormat::Maildir => *flags - Flags::JUNK - Flags::NOT_JUNK,
                };
                (raw.clone(), flags, kw.clone())
            })
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(queued(&env.store, target), expected, "{format:?}");

        // Exporting again into the same place refuses to overwrite.
        assert!(
            export(
                &env.store,
                &ExportScope {
                    account: None,
                    folder: Some(inbox.folder),
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
    let inbox = imap_target(&env, "Inbox");
    let sub = UploadTarget {
        account: inbox.account,
        folder: folder(
            &env.store,
            inbox.account,
            Some(inbox.folder),
            "Sub/One",
            Some("Inbox.Sub_One"),
        ),
    };
    for target in [inbox, sub] {
        store_synced(&env, target, &[(message(1, ""), Flags::SEEN, vec![])]);
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
    assert!(out.join("Work").join("Inbox").is_file());
    assert!(out.join("Work").join("Inbox.sbd").join("Sub_One").is_file());
    env.close();
}

#[test]
fn maildir_upload_with_keywords() {
    let env = Env::new();
    let target = imap_target(&env, "Mail");
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
    fs::write(root.join("new").join("999.new.host"), message(4, "")).unwrap();

    let stats = upload_maildir(&env.store, &root, target, "cli", &NoProgress).unwrap();
    assert_eq!(stats.queued, 3);
    let uploads = queued(&env.store, target);
    assert_eq!(
        uploads[0],
        (
            message(1, ""),
            Flags::SEEN | Flags::FLAGGED,
            vec!["projekt".to_owned()]
        )
    );
    assert_eq!(uploads[2].1, Flags::empty());

    let again = upload_maildir(&env.store, &root, target, "cli", &NoProgress).unwrap();
    assert_eq!((again.queued, again.duplicates), (0, 3));
    assert!(
        upload_maildir(
            &env.store,
            &env.scratch("empty"),
            target,
            "cli",
            &NoProgress
        )
        .is_err()
    );
    env.close();
}

#[test]
fn truncated_and_garbage_mbox_files_upload_what_they_can() {
    let env = Env::new();
    let target = imap_target(&env, "INBOX");
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
    let stats = upload_mbox(&env.store, &path, target, "cli", &NoProgress).unwrap();
    assert_eq!(stats.messages, 3);
    assert_eq!(stats.queued, 3);
    assert_eq!(stats.skipped_bytes, 17);
    assert_eq!(stats.bytes, bytes.len() as u64);
    env.close();
}

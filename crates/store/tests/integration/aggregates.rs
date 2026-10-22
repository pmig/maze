//! `thread` / `folder_thread` / folder counts stay equal to a naive recomputation under
//! random operation sequences.

use std::collections::BTreeMap;
use std::path::Path;

use mazemail_types::{AccountId, Flags, FolderId, MessageId, ThreadId};
use proptest::prelude::*;
use rusqlite::Connection;
use store::{
    AccountKind, NewFolder, NewLocation, NewMessage, Queries, Store, ThreadAssignment, WriteTx,
};

use crate::common;

#[derive(Debug, Clone)]
enum Op {
    Insert {
        folders: Vec<(usize, u8)>,
        thread: u8,
        date: i64,
        attachments: bool,
    },
    SetFlags {
        message: usize,
        add: u8,
        remove: u8,
    },
    ServerFlags {
        folder: usize,
        uid_pick: usize,
        flags: u8,
    },
    AddLocation {
        message: usize,
        folder: usize,
        flags: u8,
    },
    RemoveLocation {
        message: usize,
        folder: usize,
    },
    Move {
        message: usize,
        from: usize,
        to: usize,
    },
    Expunge {
        folder: usize,
        uid_pick: usize,
    },
    Merge {
        a: usize,
        b: usize,
    },
    SetThread {
        message: usize,
        thread: usize,
    },
    Split {
        message: usize,
    },
    Delete {
        message: usize,
    },
    Purge,
    Reset {
        folder: usize,
    },
}

fn flags(bits: u8) -> Flags {
    // Seen, answered, flagged, deleted.
    Flags::from_bits_truncate(u32::from(bits & 0b1111))
}

fn op() -> impl Strategy<Value = Op> {
    let folder = 0..4usize;
    prop_oneof![
        4 => (
            proptest::collection::vec((0..4usize, any::<u8>()), 0..3),
            any::<u8>(),
            0..50i64,
            any::<bool>()
        )
            .prop_map(|(folders, thread, date, attachments)| Op::Insert {
                folders,
                thread,
                date,
                attachments
            }),
        2 => (any::<usize>(), any::<u8>(), any::<u8>())
            .prop_map(|(message, add, remove)| Op::SetFlags { message, add, remove }),
        2 => (folder.clone(), any::<usize>(), any::<u8>())
            .prop_map(|(folder, uid_pick, flags)| Op::ServerFlags { folder, uid_pick, flags }),
        1 => (any::<usize>(), folder.clone(), any::<u8>())
            .prop_map(|(message, folder, flags)| Op::AddLocation { message, folder, flags }),
        1 => (any::<usize>(), folder.clone())
            .prop_map(|(message, folder)| Op::RemoveLocation { message, folder }),
        1 => (any::<usize>(), folder.clone(), folder.clone())
            .prop_map(|(message, from, to)| Op::Move { message, from, to }),
        1 => (folder.clone(), any::<usize>())
            .prop_map(|(folder, uid_pick)| Op::Expunge { folder, uid_pick }),
        1 => (any::<usize>(), any::<usize>()).prop_map(|(a, b)| Op::Merge { a, b }),
        1 => (any::<usize>(), any::<usize>())
            .prop_map(|(message, thread)| Op::SetThread { message, thread }),
        1 => any::<usize>().prop_map(|message| Op::Split { message }),
        1 => any::<usize>().prop_map(|message| Op::Delete { message }),
        1 => Just(Op::Purge),
        1 => folder.prop_map(|folder| Op::Reset { folder }),
    ]
}

struct World {
    account: AccountId,
    folders: Vec<FolderId>,
    next_uid: u32,
}

fn apply(w: &mut WriteTx<'_>, world: &mut World, op: &Op) -> store::Result<()> {
    let conn_messages: Vec<MessageId> = w
        .message_ids_in_scope(&store::MessageScope::Account(world.account))?
        .into_iter()
        .collect();
    let pick_message = |i: usize| conn_messages.get(i % conn_messages.len().max(1)).copied();
    let threads: Vec<ThreadId> = {
        let mut t: Vec<ThreadId> = Vec::new();
        for m in &conn_messages {
            if let Some(d) = w.message(*m)?
                && !t.contains(&d.thread)
            {
                t.push(d.thread);
            }
        }
        t
    };
    let pick_thread = |i: usize| threads.get(i % threads.len().max(1)).copied();
    let folder = |i: usize| world.folders[i % world.folders.len()];
    match op {
        Op::Insert {
            folders,
            thread,
            date,
            attachments,
        } => {
            let mut m = NewMessage::new(world.account, *date);
            m.has_attachments = *attachments;
            m.thread = match (thread % 3, pick_thread(*thread as usize)) {
                (0, Some(t)) => ThreadAssignment::Existing(t),
                _ => ThreadAssignment::New,
            };
            let mut seen = Vec::new();
            for (f, bits) in folders {
                let f = folder(*f);
                if seen.contains(&f) {
                    continue;
                }
                seen.push(f);
                world.next_uid += 1;
                // Folder index 3 is a Gmail-style label without UIDs.
                let uid = (f != world.folders[3]).then_some(world.next_uid);
                m.locations.push(NewLocation::new(f, uid, flags(*bits)));
            }
            w.upsert_message(&m)?;
        }
        Op::SetFlags {
            message,
            add,
            remove,
        } => {
            if let Some(m) = pick_message(*message) {
                w.set_flags(&[m], flags(*add), flags(*remove))?;
            }
        }
        Op::ServerFlags {
            folder: f,
            uid_pick,
            flags: bits,
        } => {
            let uids = w.folder_uids(folder(*f))?;
            if !uids.is_empty() {
                let (uid, _) = uids[uid_pick % uids.len()];
                w.update_location_flags(folder(*f), uid, flags(*bits), None, None)?;
            }
        }
        Op::AddLocation {
            message,
            folder: f,
            flags: bits,
        } => {
            if let Some(m) = pick_message(*message) {
                world.next_uid += 1;
                let f = folder(*f);
                let uid = (f != world.folders[3]).then_some(world.next_uid);
                w.add_location(m, &NewLocation::new(f, uid, flags(*bits)))?;
            }
        }
        Op::RemoveLocation { message, folder: f } => {
            if let Some(m) = pick_message(*message) {
                w.remove_location(m, folder(*f))?;
            }
        }
        Op::Move { message, from, to } => {
            if let Some(m) = pick_message(*message) {
                let (from, to) = (folder(*from), folder(*to));
                let has_from = w
                    .message(m)?
                    .is_some_and(|d| d.locations.iter().any(|l| l.folder == from));
                let has_to = w
                    .message(m)?
                    .is_some_and(|d| d.locations.iter().any(|l| l.folder == to));
                if has_from && !has_to && from != to {
                    w.move_location(m, from, to, None)?;
                }
            }
        }
        Op::Expunge {
            folder: f,
            uid_pick,
        } => {
            let uids = w.folder_uids(folder(*f))?;
            if !uids.is_empty() {
                let (uid, _) = uids[uid_pick % uids.len()];
                w.expunge(folder(*f), &[uid])?;
            }
        }
        Op::Merge { a, b } => {
            if let (Some(a), Some(b)) = (pick_thread(*a), pick_thread(*b)) {
                w.merge_threads(a, b)?;
            }
        }
        Op::SetThread { message, thread } => {
            if let (Some(m), Some(t)) = (pick_message(*message), pick_thread(*thread)) {
                w.set_message_thread(m, t)?;
            }
        }
        Op::Split { message } => {
            if let Some(m) = pick_message(*message) {
                w.split_message_into_new_thread(m)?;
            }
        }
        Op::Delete { message } => {
            if let Some(m) = pick_message(*message) {
                w.delete_message(m)?;
            }
        }
        Op::Purge => {
            w.purge_detached(world.account, i64::MAX)?;
        }
        Op::Reset { folder: f } => {
            w.reset_folder(folder(*f), 1)?;
        }
    }
    Ok(())
}

/// Folder counts, `folder_thread` and `thread` must equal a recomputation with plain
/// GROUP BY queries over the base tables.
fn check(db: &Path) {
    let conn = Connection::open(db).unwrap();
    let key = common::vault(1).sqlcipher_pragma_key();
    conn.execute_batch(&format!("PRAGMA key = \"{}\";", key.expose_secret()))
        .unwrap();

    let query = |sql: &str| -> BTreeMap<Vec<i64>, Vec<i64>> {
        let mut stmt = conn.prepare(sql).unwrap();
        let cols = stmt.column_count();
        stmt.query_map([], |r| {
            let row: Vec<i64> = (0..cols)
                .map(|i| r.get::<_, Option<i64>>(i).map(|v| v.unwrap_or(0)))
                .collect::<rusqlite::Result<_>>()?;
            Ok(row)
        })
        .unwrap()
        .map(|r| {
            let r = r.unwrap();
            (r[..2].to_vec(), r[2..].to_vec())
        })
        .collect()
    };

    let stored = query(
        "SELECT folder_id, thread_id, message_count, unread_count, flagged, attachment_count, last_date
         FROM folder_thread",
    );
    let naive = query(
        "SELECT ml.folder_id, m.thread_id, count(*), sum((ml.flags & 1) = 0), sum((ml.flags & 4) != 0),
                sum(m.has_attachments != 0), max(m.date)
         FROM message_location ml JOIN message m ON m.id = ml.message_id
         GROUP BY ml.folder_id, m.thread_id",
    );
    assert_eq!(stored, naive, "folder_thread");

    let stored = query("SELECT id, 0, total, unread FROM folder");
    let naive = query(
        "SELECT f.id, 0, (SELECT count(*) FROM message_location WHERE folder_id = f.id),
                (SELECT count(*) FROM message_location WHERE folder_id = f.id AND (flags & 1) = 0)
         FROM folder f",
    );
    assert_eq!(stored, naive, "folder counts");

    let stored = query(
        "SELECT id, 0, message_count, unread_count, flagged, attachment_count,
                CASE WHEN message_count > 0 THEN last_date ELSE 0 END,
                CASE WHEN message_count > 0 THEN last_message_id ELSE 0 END
         FROM thread",
    );
    let naive = query(
        "WITH per_message AS (
           SELECT m.id, m.thread_id, m.date, m.has_attachments,
                  min(ml.flags & 1) AS seen_all, max((ml.flags & 4) != 0) AS flagged
           FROM message m JOIN message_location ml ON ml.message_id = m.id GROUP BY m.id),
         threads AS (SELECT DISTINCT thread_id AS id FROM message)
         SELECT t.id, 0,
           (SELECT count(*) FROM per_message p WHERE p.thread_id = t.id),
           (SELECT count(*) FROM per_message p WHERE p.thread_id = t.id AND p.seen_all = 0),
           (SELECT count(*) FROM per_message p WHERE p.thread_id = t.id AND p.flagged),
           (SELECT count(*) FROM per_message p WHERE p.thread_id = t.id AND p.has_attachments != 0),
           COALESCE((SELECT max(date) FROM per_message p WHERE p.thread_id = t.id), 0),
           COALESCE((SELECT id FROM per_message p WHERE p.thread_id = t.id
                     ORDER BY date DESC, id DESC LIMIT 1), 0)
         FROM threads t",
    );
    assert_eq!(stored, naive, "thread aggregates");
}

fn setup(dir: &Path) -> (Store, World) {
    let store = common::open(dir);
    let f = common::fixture(&store, AccountKind::Gmail);
    let label = store
        .write_blocking(move |w| {
            w.create_folder(&NewFolder::new(f.account, "Work", "Work").label())
        })
        .unwrap();
    let world = World {
        account: f.account,
        folders: vec![f.inbox, f.archive, f.sent, label],
        next_uid: 0,
    };
    (store, world)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn aggregates_match_naive_recomputation(
        ops in proptest::collection::vec(op(), 1..40),
        batch in 1..5usize,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let (store, world) = setup(tmp.path());
        let world = std::sync::Arc::new(std::sync::Mutex::new(world));
        // Several ops per job, so aggregates are also checked across a job's changes.
        for chunk in ops.chunks(batch) {
            let chunk = chunk.to_vec();
            let world = world.clone();
            store
                .write_blocking(move |w| {
                    let mut world = world.lock().unwrap();
                    for op in &chunk {
                        apply(w, &mut world, op)?;
                    }
                    Ok(())
                })
                .unwrap();
        }
        check(&tmp.path().join("mail/mazemail.db"));
    }
}

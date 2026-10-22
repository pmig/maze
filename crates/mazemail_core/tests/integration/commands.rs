//! Every command: optimistic local effect, pending op, audit, undo; agent permissions.

use std::sync::Arc;

use mazemail_core::{
    Authorization, CommandAuthorizer, CommandOutcome, CoreHandle, Error, MailCommand, Origin,
    RefreshScope, TokenId,
};
use mazemail_types::{AccountId, Flags, FolderId, MessageId};
use serde_json::Value;
use store::{AuditQuery, MessageDetail, Queries};

use crate::common::{self, Fixture, Place, TestCore};

fn detail(core: &CoreHandle, id: MessageId) -> MessageDetail {
    core.store().reader().unwrap().message(id).unwrap().unwrap()
}

fn folders_of(core: &CoreHandle, id: MessageId) -> Vec<FolderId> {
    let mut folders: Vec<FolderId> = detail(core, id)
        .locations
        .iter()
        .map(|l| l.folder)
        .collect();
    folders.sort();
    folders
}

/// The account's pending ops as `(kind, payload)`.
fn ops(core: &CoreHandle, account: AccountId) -> Vec<(String, Value)> {
    core.store()
        .reader()
        .unwrap()
        .pending_ops(account)
        .unwrap()
        .into_iter()
        .map(|op| {
            let payload: Value = serde_json::from_str(&op.payload_json).unwrap();
            assert_eq!(payload["op"], op.kind.as_str());
            assert_eq!(payload["v"], 1);
            (op.kind, payload)
        })
        .collect()
}

fn last_audit(core: &CoreHandle) -> (String, String, Value) {
    let entry = core
        .store()
        .reader()
        .unwrap()
        .audit_log(&AuditQuery {
            limit: 1,
            ..AuditQuery::default()
        })
        .unwrap()
        .remove(0);
    let detail = entry
        .detail_json
        .as_deref()
        .map(|d| serde_json::from_str(d).unwrap())
        .unwrap_or(Value::Null);
    (entry.origin, entry.action, detail)
}

async fn setup() -> (TestCore, Fixture, MessageId, MessageId) {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let raw = |n: u32| {
        common::raw_message(
            &format!("c{n}@example.com"),
            "anna@example.com",
            "me@work.example",
            &format!("Message {n}"),
            "Mon, 6 Oct 2025 10:00:00 +0000",
            "Body",
        )
    };
    let (a, _) = common::ingest(&t.core, Place::new(f.work, f.inbox, 11), raw(1)).await;
    let (b, _) = common::ingest(&t.core, Place::new(f.work, f.inbox, 12), raw(2)).await;
    (t, f, a, b)
}

#[tokio::test(flavor = "multi_thread")]
async fn set_flags_applies_queues_audits_and_undoes() {
    let (t, f, a, b) = setup().await;
    // `b` is already read: undo must leave it read.
    t.core
        .apply(
            MailCommand::SetFlags {
                ids: vec![b],
                add: Flags::SEEN,
                remove: Flags::empty(),
            },
            Origin::Cli,
        )
        .await
        .unwrap();
    let receipt = t
        .core
        .apply(
            MailCommand::SetFlags {
                ids: vec![a, b],
                add: Flags::SEEN | Flags::FLAGGED,
                remove: Flags::empty(),
            },
            Origin::User,
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, CommandOutcome::Applied);
    assert!(receipt.undoable);
    assert_eq!(receipt.ops.len(), 1);
    assert_eq!(receipt.messages, vec![a, b]);
    assert!(
        detail(&t.core, a)
            .effective_flags()
            .contains(Flags::SEEN | Flags::FLAGGED)
    );

    let queued = ops(&t.core, f.work);
    let (kind, payload) = queued.last().unwrap();
    assert_eq!(kind, "set_flags");
    assert_eq!(payload["add"], serde_json::json!(["\\Seen", "\\Flagged"]));
    assert_eq!(payload["targets"][0]["mailbox"], "INBOX");
    assert_eq!(payload["targets"][0]["uid"], 11);
    assert_eq!(payload["targets"][0]["rfc_message_id"], "c1@example.com");
    assert!(t.sync.op_wakeups.lock().unwrap().contains(&f.work));

    let (origin, action, audit) = last_audit(&t.core);
    assert_eq!((origin.as_str(), action.as_str()), ("user", "set_flags"));
    assert_eq!(audit["messages"], 2);
    assert!(!audit.to_string().contains("anna@"), "audit holds only ids");

    // Rows see the change right away.
    let thread = detail(&t.core, a).thread;
    let rows = t.core.rows(&[thread], Some(f.inbox)).unwrap();
    assert_eq!(rows[0].unread_count, 0);
    assert!(rows[0].flagged);

    assert!(t.core.can_undo());
    let undo = t.core.undo().await.unwrap().unwrap();
    assert!(!undo.undoable);
    assert_eq!(detail(&t.core, a).effective_flags(), Flags::empty());
    assert_eq!(detail(&t.core, b).effective_flags(), Flags::SEEN);
    let rows = t.core.rows(&[thread], Some(f.inbox)).unwrap();
    assert_eq!(rows[0].unread_count, 1);
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(kind, "set_flags");
    assert!(
        payload["remove"]
            .as_array()
            .unwrap()
            .contains(&"\\Flagged".into())
    );
    assert_eq!(last_audit(&t.core).1, "undo");
    assert!(!t.core.can_undo());
    assert!(t.core.undo().await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn tags_are_keywords_and_undo_restores_them() {
    let (t, f, a, _) = setup().await;
    // Thunderbird's default tags exist from the first launch.
    let tag = t
        .core
        .store()
        .reader()
        .unwrap()
        .tag_by_keyword("$label1")
        .unwrap()
        .unwrap()
        .id;
    t.core
        .apply(
            MailCommand::SetTags {
                ids: vec![a],
                add: vec![tag],
                remove: Vec::new(),
            },
            Origin::User,
        )
        .await
        .unwrap();
    assert_eq!(detail(&t.core, a).tags, vec![tag]);
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(kind, "set_flags");
    assert_eq!(payload["add_keywords"], serde_json::json!(["$label1"]));
    t.core.undo().await.unwrap().unwrap();
    assert!(detail(&t.core, a).tags.is_empty());
    let (_, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(payload["remove_keywords"], serde_json::json!(["$label1"]));

    let unknown = t
        .core
        .apply(
            MailCommand::SetTags {
                ids: vec![a],
                add: vec![mazemail_types::TagId(999)],
                remove: Vec::new(),
            },
            Origin::User,
        )
        .await;
    assert!(matches!(
        unknown,
        Err(Error::NotFound { entity: "tag", .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn move_copy_and_their_undo() {
    let (t, f, a, b) = setup().await;
    let receipt = t
        .core
        .apply(
            MailCommand::Move {
                ids: vec![a],
                to: f.projects,
                from: None,
            },
            Origin::User,
        )
        .await
        .unwrap();
    assert!(receipt.undoable);
    assert_eq!(folders_of(&t.core, a), vec![f.projects]);
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(kind, "move");
    assert_eq!(payload["to"]["remote_name"], "Projects");
    assert_eq!(payload["targets"][0]["mailbox"], "INBOX");
    assert_eq!(payload["targets"][0]["uid"], 11);

    t.core.undo().await.unwrap().unwrap();
    assert_eq!(folders_of(&t.core, a), vec![f.inbox]);
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(kind, "move");
    assert_eq!(payload["to"]["remote_name"], "INBOX");
    assert_eq!(payload["targets"][0]["mailbox"], "Projects");

    t.core
        .apply(
            MailCommand::Copy {
                ids: vec![b],
                to: f.projects,
            },
            Origin::User,
        )
        .await
        .unwrap();
    let mut expected = vec![f.inbox, f.projects];
    expected.sort();
    assert_eq!(folders_of(&t.core, b), expected);
    assert_eq!(ops(&t.core, f.work).pop().unwrap().0, "copy");
    t.core.undo().await.unwrap().unwrap();
    assert_eq!(folders_of(&t.core, b), vec![f.inbox]);
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(
        (kind.as_str(), &payload["permanent"]),
        ("delete", &Value::Bool(true))
    );

    // Across accounts is refused and changes nothing.
    let before = ops(&t.core, f.work).len();
    let refused = t
        .core
        .apply(
            MailCommand::Move {
                ids: vec![a],
                to: f.g_label,
                from: None,
            },
            Origin::User,
        )
        .await;
    assert!(matches!(refused, Err(Error::InvalidCommand(_))));
    assert_eq!(ops(&t.core, f.work).len(), before);
    assert_eq!(folders_of(&t.core, a), vec![f.inbox]);
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_uses_the_identity_folder_and_granularity() {
    let (t, f, a, _) = setup().await;
    t.core
        .apply(MailCommand::Archive { ids: vec![a] }, Origin::User)
        .await
        .unwrap();
    // Yearly granularity: Archive/2025, created locally and on the server.
    let folders = t
        .core
        .store()
        .reader()
        .unwrap()
        .folders(Some(f.work))
        .unwrap();
    let year = folders
        .iter()
        .find(|x| x.parent == Some(f.archive) && x.display_name == "2025")
        .expect("year folder created");
    assert_eq!(year.remote_name.as_deref(), Some("Archive/2025"));
    assert_eq!(folders_of(&t.core, a), vec![year.id]);
    let queued = ops(&t.core, f.work);
    let kinds: Vec<&str> = queued.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(kinds, vec!["create_folder", "archive"]);
    assert_eq!(queued[1].1["target"]["kind"], "folder");
    assert_eq!(
        queued[1].1["target"]["folder"]["remote_name"],
        "Archive/2025"
    );
    assert_eq!(queued[1].1["targets"][0]["uid"], 11);

    t.core.undo().await.unwrap().unwrap();
    assert_eq!(folders_of(&t.core, a), vec![f.inbox]);
}

#[tokio::test(flavor = "multi_thread")]
async fn gmail_archive_removes_the_inbox_label() {
    let t = common::open().await;
    let f = common::fixture(&t.core).await;
    let raw = common::raw_message(
        "g1@example.com",
        "anna@example.com",
        "me@gmail.example",
        "Gmail message",
        "Mon, 6 Oct 2025 10:00:00 +0000",
        "Body",
    );
    let (id, _) = common::ingest(&t.core, Place::new(f.gmail, f.g_inbox, 5).gmail(1001), raw).await;
    let all = f.g_all;
    t.core
        .store()
        .write(move |w| {
            w.add_location(id, &store::NewLocation::new(all, Some(900), Flags::empty()))
        })
        .await
        .unwrap();

    t.core
        .apply(MailCommand::Archive { ids: vec![id] }, Origin::User)
        .await
        .unwrap();
    assert_eq!(folders_of(&t.core, id), vec![f.g_all]);
    let (kind, payload) = ops(&t.core, f.gmail).pop().unwrap();
    assert_eq!(kind, "archive");
    assert_eq!(payload["target"]["kind"], "gmail_remove_inbox");
    assert_eq!(payload["targets"][0]["gm_msgid"], 1001);

    t.core.undo().await.unwrap().unwrap();
    let mut expected = vec![f.g_inbox, f.g_all];
    expected.sort();
    assert_eq!(folders_of(&t.core, id), expected);
    assert_eq!(ops(&t.core, f.gmail).pop().unwrap().0, "copy");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_moves_to_trash_and_deletes_for_good_from_there() {
    let (t, f, a, b) = setup().await;
    t.core
        .apply(
            MailCommand::Delete {
                ids: vec![a],
                permanent: false,
            },
            Origin::User,
        )
        .await
        .unwrap();
    assert_eq!(folders_of(&t.core, a), vec![f.trash]);
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(kind, "delete");
    assert_eq!(payload["permanent"], false);
    assert_eq!(payload["trash"]["remote_name"], "Trash");

    t.core.undo().await.unwrap().unwrap();
    assert_eq!(folders_of(&t.core, a), vec![f.inbox]);

    // Delete twice: the second time it is already in Trash, so it goes for good.
    for _ in 0..2 {
        t.core
            .apply(
                MailCommand::Delete {
                    ids: vec![a],
                    permanent: false,
                },
                Origin::User,
            )
            .await
            .unwrap();
    }
    assert!(
        t.core
            .store()
            .reader()
            .unwrap()
            .message(a)
            .unwrap()
            .is_none()
    );
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(
        (kind.as_str(), &payload["permanent"]),
        ("delete", &Value::Bool(true))
    );
    assert_eq!(payload["targets"][0]["mailbox"], "Trash");
    assert!(!t.core.can_undo(), "a permanent delete can't be undone");

    let receipt = t
        .core
        .apply(
            MailCommand::Delete {
                ids: vec![b],
                permanent: true,
            },
            Origin::User,
        )
        .await
        .unwrap();
    assert!(!receipt.undoable);
    assert!(
        t.core
            .store()
            .reader()
            .unwrap()
            .message(b)
            .unwrap()
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn folder_commands_and_mark_folder_read() {
    let (t, f, a, b) = setup().await;
    let created = t
        .core
        .apply(
            MailCommand::CreateFolder {
                account: f.work,
                parent: Some(f.projects),
                name: "Q3".to_owned(),
            },
            Origin::User,
        )
        .await
        .unwrap();
    let q3 = created.created_folder.unwrap();
    assert!(!created.undoable);
    let reader = || t.core.store().reader().unwrap();
    assert_eq!(
        reader().folder(q3).unwrap().unwrap().remote_name.as_deref(),
        Some("Projects/Q3")
    );
    assert_eq!(ops(&t.core, f.work).pop().unwrap().0, "create_folder");

    t.core
        .apply(
            MailCommand::RenameFolder {
                folder: q3,
                name: "Q4".to_owned(),
            },
            Origin::User,
        )
        .await
        .unwrap();
    let renamed = reader().folder(q3).unwrap().unwrap();
    assert_eq!(renamed.display_name, "Q4");
    assert_eq!(renamed.remote_name.as_deref(), Some("Projects/Q4"));
    let (kind, payload) = ops(&t.core, f.work).pop().unwrap();
    assert_eq!(kind, "rename_folder");
    assert_eq!(payload["from_name"], "Projects/Q3");

    // Renaming the parent renames the child's server name too.
    t.core
        .apply(
            MailCommand::RenameFolder {
                folder: f.projects,
                name: "Work Projects".to_owned(),
            },
            Origin::User,
        )
        .await
        .unwrap();
    assert_eq!(
        reader().folder(q3).unwrap().unwrap().remote_name.as_deref(),
        Some("Work Projects/Q4")
    );

    let special = t
        .core
        .apply(MailCommand::DeleteFolder { folder: f.inbox }, Origin::User)
        .await;
    assert!(matches!(special, Err(Error::InvalidCommand(_))));
    let bad_name = t
        .core
        .apply(
            MailCommand::CreateFolder {
                account: f.work,
                parent: None,
                name: "a/b".to_owned(),
            },
            Origin::User,
        )
        .await;
    assert!(matches!(bad_name, Err(Error::InvalidCommand(_))));

    t.core
        .apply(MailCommand::DeleteFolder { folder: q3 }, Origin::User)
        .await
        .unwrap();
    assert!(reader().folder(q3).unwrap().is_none());
    assert_eq!(ops(&t.core, f.work).pop().unwrap().0, "delete_folder");

    let receipt = t
        .core
        .apply(
            MailCommand::MarkFolderRead { folder: f.inbox },
            Origin::User,
        )
        .await
        .unwrap();
    let mut marked = receipt.messages.clone();
    marked.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(marked, expected);
    assert_eq!(reader().folder(f.inbox).unwrap().unwrap().unread, 0);
    t.core.undo().await.unwrap().unwrap();
    assert_eq!(reader().folder(f.inbox).unwrap().unwrap().unread, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_goes_to_sync() {
    let (t, f, _, _) = setup().await;
    let receipt = t
        .core
        .apply(
            MailCommand::Refresh {
                scope: RefreshScope::Folder(f.inbox),
            },
            Origin::User,
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, CommandOutcome::Requested);
    assert_eq!(
        *t.sync.refreshes.lock().unwrap(),
        vec![RefreshScope::Folder(f.inbox)]
    );
    assert_eq!(last_audit(&t.core).1, "refresh");
}

/// Allows the first token, wants review for the second, refuses the rest.
#[derive(Debug)]
struct Policy {
    allow: TokenId,
    propose: TokenId,
}

impl CommandAuthorizer for Policy {
    fn authorize(&self, token: TokenId, _command: &MailCommand) -> Authorization {
        match token {
            t if t == self.allow => Authorization::Allow,
            t if t == self.propose => Authorization::Propose {
                title: "Clean up".to_owned(),
                reason: Some("Looks like spam".to_owned()),
            },
            _ => Authorization::Deny {
                reason: "read-only token".to_owned(),
            },
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_commands_go_through_the_authorizer() {
    // Default: no agent access.
    let (t, f, a, _) = setup().await;
    let denied = t
        .core
        .apply(
            MailCommand::SetFlags {
                ids: vec![a],
                add: Flags::SEEN,
                remove: Flags::empty(),
            },
            Origin::Agent { token: TokenId(1) },
        )
        .await;
    assert!(matches!(denied, Err(Error::PermissionDenied { .. })));
    assert_eq!(last_audit(&t.core).1, "denied");
    assert!(ops(&t.core, f.work).is_empty());

    let tokens = t
        .core
        .store()
        .write(|w| {
            (0..3)
                .map(|n| {
                    w.create_api_token(&store::NewApiToken {
                        hash: store::token_hash(&format!("mazemail_s_test{n}")),
                        profile: "triage".to_owned(),
                        scope_json: "{}".to_owned(),
                        expires: None,
                        label: None,
                    })
                })
                .collect::<store::Result<Vec<_>>>()
        })
        .await
        .unwrap();
    t.core.shutdown().await.unwrap();
    let (allow, propose, refuse) = (tokens[0], tokens[1], tokens[2]);

    let core = CoreHandle::open(
        common::config(t.dir.path(), &t.keys).with_authorizer(Arc::new(Policy { allow, propose })),
    )
    .await
    .unwrap();
    let delete = MailCommand::Delete {
        ids: vec![a],
        permanent: false,
    };
    let proposed = core
        .apply(delete.clone(), Origin::Agent { token: propose })
        .await
        .unwrap();
    let CommandOutcome::Proposed(proposal) = proposed.outcome else {
        panic!("expected a proposal, got {:?}", proposed.outcome);
    };
    assert_eq!(folders_of(&core, a), vec![f.inbox], "nothing applied");
    let stored = core
        .store()
        .reader()
        .unwrap()
        .proposal(proposal)
        .unwrap()
        .unwrap();
    assert_eq!(stored.items.len(), 1);
    let command: MailCommand = serde_json::from_str(&stored.items[0].command_json).unwrap();
    assert_eq!(command, delete);
    assert_eq!(last_audit(&core).1, "proposed");

    let refused = core
        .apply(delete.clone(), Origin::Agent { token: refuse })
        .await;
    assert!(
        matches!(refused, Err(Error::PermissionDenied { reason }) if reason == "read-only token")
    );

    let applied = core
        .apply(delete, Origin::Agent { token: allow })
        .await
        .unwrap();
    assert!(
        !applied.undoable,
        "agent commands aren't on the user's undo"
    );
    assert_eq!(folders_of(&core, a), vec![f.trash]);
    let op = core
        .store()
        .reader()
        .unwrap()
        .pending_ops(f.work)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(op.origin, format!("agent:{allow}"));
    assert_eq!(last_audit(&core).0, format!("agent:{allow}"));
    core.shutdown().await.unwrap();
}

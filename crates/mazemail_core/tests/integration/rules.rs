//! Message rules (ADR-029): new mail is filtered through the normal command path,
//! once; rules run on a folder on request.

use mazemail_core::{CoreHandle, FetchPriority, NewRule};
use mazemail_types::{Flags, FolderId, MessageId};
use store::{AuditQuery, FolderSyncState, Queries as _, RuleInput, RuleTrigger};

use crate::common::{Fixture, Place, fixture, ingest, open, raw_message};

const DATE: &str = "Tue, 14 Oct 2025 09:30:00 +0000";

async fn mail(core: &CoreHandle, fx: &Fixture, uid: u32, from: &str, subject: &str) -> MessageId {
    ingest(
        core,
        Place::new(fx.work, fx.inbox, uid),
        raw_message(
            &format!("m{uid}@example.net"),
            from,
            "me@work.example",
            subject,
            DATE,
            "Hello there.",
        ),
    )
    .await
    .0
}

async fn synced(core: &CoreHandle, folder: FolderId, uidnext: u32) {
    core.store()
        .write(move |w| {
            w.set_folder_sync_state(
                folder,
                FolderSyncState {
                    uidvalidity: Some(7),
                    uidnext: Some(uidnext),
                    highestmodseq: None,
                },
            )
        })
        .await
        .unwrap();
}

async fn add(
    core: &CoreHandle,
    fx: &Fixture,
    name: &str,
    condition: &str,
    actions: &str,
) -> store::RuleId {
    let rule = rules::Rule::parse(condition, actions).unwrap();
    core.add_rule(NewRule {
        account: Some(fx.work),
        name: name.to_owned(),
        rule,
        enabled: true,
    })
    .await
    .unwrap()
}

fn folders_of(core: &CoreHandle, id: MessageId) -> Vec<FolderId> {
    let detail = core.store().reader().unwrap().message(id).unwrap().unwrap();
    detail.locations.iter().map(|l| l.folder).collect()
}

#[tokio::test]
async fn new_mail_is_filtered_once_through_the_command_path() {
    let t = open().await;
    let core = &t.core;
    let fx = fixture(core).await;
    // Thunderbird's default tags exist from the start.
    let tag = core
        .store()
        .reader()
        .unwrap()
        .tag_by_keyword("$label1")
        .unwrap()
        .unwrap()
        .id;
    let old = mail(core, &fx, 1, "News <news@example.org>", "Old news").await;
    synced(core, fx.inbox, 2).await;

    let news = add(
        core,
        &fx,
        "News",
        r#"{"match":"all","terms":[{"field":"from","op":"contains","value":"news@"}]}"#,
        r#"[{"action":"move_to_folder","value":"Projects"},{"action":"add_tag","value":"$label1"}]"#,
    )
    .await;
    let stop = add(
        core,
        &fx,
        "Boss",
        r#"{"match":"any","terms":[{"field":"subject","op":"begins_with","value":"[boss]"}]}"#,
        r#"[{"action":"mark_flagged"},{"action":"stop"}]"#,
    )
    .await;
    let later = add(
        core,
        &fx,
        "Everything read",
        r#"{"match":"all_messages"}"#,
        r#"[{"action":"mark_read"}]"#,
    )
    .await;

    // The first pass starts the cursor after what the first sync found: old mail stays.
    let first = core.run_arrival_rules(fx.work).await.unwrap();
    assert_eq!(first.examined, 0);
    assert_eq!(folders_of(core, old), vec![fx.inbox]);

    let fresh = mail(core, &fx, 2, "News <news@example.org>", "Fresh news").await;
    let boss = mail(core, &fx, 3, "Boss <boss@example.org>", "[Boss] Today").await;
    let other = mail(core, &fx, 4, "Anna <anna@example.org>", "Lunch").await;
    let report = core.run_arrival_rules(fx.work).await.unwrap();
    assert_eq!(report.examined, 3);
    assert_eq!(report.matched, 3);
    assert_eq!(report.failed, 0);

    let reader = core.store().reader().unwrap();
    // Tagged, then moved (the move ends the evaluation: "Everything read" didn't run).
    let detail = reader.message(fresh).unwrap().unwrap();
    assert_eq!(folders_of(core, fresh), vec![fx.projects]);
    assert!(detail.tags.contains(&tag));
    assert!(!detail.effective_flags().contains(Flags::SEEN));
    // Stop: flagged, not marked read.
    let flags = reader.message(boss).unwrap().unwrap().effective_flags();
    assert!(flags.contains(Flags::FLAGGED));
    assert!(!flags.contains(Flags::SEEN));
    // The catch-all rule ran on the rest.
    let flags = reader.message(other).unwrap().unwrap().effective_flags();
    assert!(flags.contains(Flags::SEEN));
    // Server ops are queued like any command's, and each action is audited.
    assert!(!reader.pending_ops(fx.work).unwrap().is_empty());
    for (rule, action) in [
        (news, "move"),
        (news, "set_tags"),
        (stop, "set_flags"),
        (later, "set_flags"),
    ] {
        let entries = reader
            .audit_log(&AuditQuery {
                origin: Some(format!("rule:{rule}")),
                limit: 10,
                ..AuditQuery::default()
            })
            .unwrap();
        assert!(
            entries.iter().any(|e| e.action == action),
            "{rule} {action}: {entries:?}"
        );
    }
    assert!(!t.sync.op_wakeups.lock().unwrap().is_empty());
    drop(reader);

    // Once: running again finds nothing new, even after a re-sync of the same mail.
    let again = core.run_arrival_rules(fx.work).await.unwrap();
    assert_eq!((again.examined, again.actions), (0, 0));

    // A disabled rule doesn't run on new mail.
    core.set_rule_enabled(later, false).await.unwrap();
    let quiet = mail(core, &fx, 5, "Carl <carl@example.org>", "Hi").await;
    let report = core.run_arrival_rules(fx.work).await.unwrap();
    assert_eq!((report.examined, report.matched), (1, 0));
    let reader = core.store().reader().unwrap();
    assert!(
        !reader
            .message(quiet)
            .unwrap()
            .unwrap()
            .effective_flags()
            .contains(Flags::SEEN)
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn body_conditions_wait_for_the_body() {
    let t = open().await;
    let core = &t.core;
    let fx = fixture(core).await;
    synced(core, fx.inbox, 1).await;
    add(
        core,
        &fx,
        "Invoices",
        r#"{"match":"all","terms":[{"field":"body","op":"contains","value":"invoice"}]}"#,
        r#"[{"action":"trash"}]"#,
    )
    .await;
    let raw = raw_message(
        "b1@example.net",
        "a@example.org",
        "me@work.example",
        "Hi",
        DATE,
        "Your invoice",
    );
    let (id, _) = ingest(
        core,
        Place::new(fx.work, fx.inbox, 1).headers_only(),
        raw.clone(),
    )
    .await;
    let held = core.run_arrival_rules(fx.work).await.unwrap();
    assert_eq!(held.examined, 0);
    assert_eq!(folders_of(core, id), vec![fx.inbox]);

    // The body arrives: the rule runs (to the Trash, never deleted for good).
    let staged = core.store().blobs().put(&raw).unwrap();
    core.store()
        .write(move |w| w.set_message_body(id, &staged, store::BodyState::Full))
        .await
        .unwrap();
    let report = core.run_arrival_rules(fx.work).await.unwrap();
    assert_eq!(report.examined, 1);
    assert_eq!(folders_of(core, id), vec![fx.trash]);
    // The runner asks for held-back bodies; without it nothing was fetched here.
    assert!(
        !t.sync
            .fetches
            .lock()
            .unwrap()
            .iter()
            .any(|(m, p)| *m == id && *p == FetchPriority::Interactive)
    );
    core.shutdown().await.unwrap();
}

#[tokio::test]
async fn rules_run_on_a_folder_on_request_and_only_runnable_ones_enable() {
    let t = open().await;
    let core = &t.core;
    let fx = fixture(core).await;
    let a = mail(core, &fx, 1, "News <news@example.org>", "One").await;
    let b = mail(core, &fx, 2, "Anna <anna@example.org>", "Two").await;
    let rule = add(
        core,
        &fx,
        "News",
        r#"{"match":"all","terms":[{"field":"from","op":"is","value":"news@example.org"}]}"#,
        r#"[{"action":"copy_to_folder","value":"projects"},{"action":"mark_read"}]"#,
    )
    .await;
    core.set_rule_enabled(rule, false).await.unwrap();

    // Disabled rules don't run on a folder, unless named.
    let none = core.run_rules(fx.inbox, None).await.unwrap();
    assert_eq!((none.examined, none.matched), (0, 0));
    let report = core.run_rules(fx.inbox, Some(rule)).await.unwrap();
    assert_eq!((report.examined, report.matched, report.actions), (2, 1, 2));
    let mut folders = folders_of(core, a);
    folders.sort();
    assert_eq!(folders, vec![fx.inbox, fx.projects]);
    assert_eq!(folders_of(core, b), vec![fx.inbox]);

    // An imported rule with a condition Maze can't evaluate stays disabled.
    let imported = core
        .store()
        .write(move |w| {
            w.create_rule(&RuleInput {
                account: None,
                name: "Old".to_owned(),
                ordinal: 9,
                enabled: false,
                trigger: RuleTrigger::ArrivalBeforeJunk,
                condition_json: r#"{"source":"thunderbird","match":"all","terms":[{"field":"thunderbird:age in days","op":"is","value":"3"}],"import_note":"n"}"#.to_owned(),
                actions_json: r#"[{"action":"mark_read"}]"#.to_owned(),
            })
        })
        .await
        .unwrap();
    let summary = core.rule(imported).unwrap();
    assert!(summary.imported && !summary.runnable);
    assert_eq!(summary.note.as_deref(), Some("n"));
    let err = core.set_rule_enabled(imported, true).await.unwrap_err();
    assert!(err.to_string().contains("can't be enabled"), "{err}");
    assert_eq!(core.rules(Some(fx.work)).unwrap().len(), 2);
    core.delete_rule(imported).await.unwrap();
    assert_eq!(core.rules(None).unwrap().len(), 1);
    core.shutdown().await.unwrap();
}

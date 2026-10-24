//! Pending ops, outbox and send log, tokens, proposals, audit log, settings rows,
//! change events and the async writer API.

use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;
use store::{
    AccountKind, AuditQuery, IdentityInput, IdentityRuleKind, MAX_OP_ATTEMPTS, NewApiToken,
    NewProposalItem, OpState, OutboxState, ProposalItemState, ProposalState, Queries, RuleInput,
    RuleTrigger, SavesSent, SignatureInput, SmtpAuth, SmtpSecurity, SmtpServerInput, TokenId,
    token_hash,
};

use crate::common::{self, message};

#[test]
fn pending_ops_replay_in_order_with_backoff() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let (first, second) = store
        .write_blocking(move |w| {
            Ok((
                w.enqueue_op(f.account, "set_flags", r#"{"uids":[1]}"#, "user")?,
                w.enqueue_op(f.account, "move", r#"{"uids":[2]}"#, "agent:3")?,
            ))
        })
        .unwrap();
    let reader = store.reader().unwrap();
    let ops = reader.pending_ops(f.account).unwrap();
    assert_eq!(
        ops.iter().map(|o| o.id).collect::<Vec<_>>(),
        vec![first, second]
    );
    assert_eq!(ops[1].origin, "agent:3");
    assert_eq!(reader.accounts_with_pending_ops().unwrap(), vec![f.account]);
    let t0 = ops[0].created;
    assert_eq!(
        reader.next_pending_op(f.account, t0).unwrap().unwrap().id,
        first
    );
    drop(reader);

    // Fail the first op: it is not due until the backoff passed, and blocks the second.
    let state = store
        .write_blocking(move |w| w.op_failed(first, "connection reset", 0))
        .unwrap();
    assert_eq!(state, OpState::Queued);
    let reader = store.reader().unwrap();
    assert_eq!(reader.next_pending_op(f.account, t0).unwrap(), None);
    let op = reader
        .next_pending_op(f.account, t0 + 3600)
        .unwrap()
        .unwrap();
    assert_eq!((op.id, op.attempts), (first, 1));
    assert_eq!(op.last_error.as_deref(), Some("connection reset"));
    drop(reader);

    // After MAX_OP_ATTEMPTS failures it is stuck and the next op runs.
    for _ in 1..MAX_OP_ATTEMPTS {
        store
            .write_blocking(move |w| w.op_failed(first, "still failing", 0))
            .unwrap();
    }
    let reader = store.reader().unwrap();
    assert_eq!(
        reader.pending_ops(f.account).unwrap()[0].state,
        OpState::Stuck
    );
    assert_eq!(
        reader
            .next_pending_op(f.account, i64::MAX)
            .unwrap()
            .unwrap()
            .id,
        second
    );
    drop(reader);

    store.write_blocking(move |w| w.op_retry(first)).unwrap();
    store.write_blocking(move |w| w.op_done(second)).unwrap();
    store.write_blocking(move |w| w.op_discard(first)).unwrap();
    assert!(
        store
            .reader()
            .unwrap()
            .pending_ops(f.account)
            .unwrap()
            .is_empty()
    );
    assert_eq!(store::retry_backoff_secs(1), 30);
    assert_eq!(store::retry_backoff_secs(20), 3600);
}

#[test]
fn outbox_send_log_and_rollback_reconciliation() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let raw = store.blobs().put(b"Message-ID: <one@x>\r\n\r\nhi").unwrap();
    let raw2 = store.blobs().put(b"Message-ID: <two@x>\r\n\r\nhi").unwrap();
    let (one, two) = store
        .write_blocking(move |w| {
            Ok((
                w.outbox_enqueue(None, &raw, "one@x", 100, None)?,
                w.outbox_enqueue(None, &raw2, "two@x", 200, None)?,
            ))
        })
        .unwrap();
    let reader = store.reader().unwrap();
    assert_eq!(reader.outbox_due(150).unwrap().len(), 1);
    assert_eq!(reader.outbox_due(250).unwrap().len(), 2);
    drop(reader);

    store
        .write_blocking(move |w| w.outbox_set_state(one, OutboxState::Sending, None))
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let receipt = runtime
        .block_on(store.record_sent(one, "one@x", None))
        .unwrap();
    assert_eq!(receipt.outbox, Some(one));
    assert!(store.sendlog().was_sent("one@x").unwrap().is_some());
    assert!(store.sendlog().was_sent("two@x").unwrap().is_none());
    let item = store.reader().unwrap().outbox_item(one).unwrap().unwrap();
    assert_eq!((item.state, item.attempts), (OutboxState::Sent, 1));

    // A rollback restores an older database where `one` is still queued.
    store
        .write_blocking(move |w| w.outbox_reschedule(two, 300))
        .unwrap();
    let reset_one =
        store.write_blocking(move |w| w.outbox_set_state(one, OutboxState::Queued, None));
    reset_one.unwrap();
    let (sent, frozen) = runtime
        .block_on(store.reconcile_outbox_after_rollback())
        .unwrap();
    assert_eq!((sent, frozen), (1, 1));
    let reader = store.reader().unwrap();
    assert_eq!(
        reader.outbox_item(one).unwrap().unwrap().state,
        OutboxState::Sent
    );
    assert_eq!(
        reader.outbox_item(two).unwrap().unwrap().state,
        OutboxState::Frozen
    );
    assert!(reader.outbox_due(i64::MAX).unwrap().is_empty());
    drop(reader);

    // The send log is its own file and survives a restart.
    runtime.block_on(store.close()).unwrap();
    let store = common::open(tmp.path());
    assert!(store.sendlog().was_sent("one@x").unwrap().is_some());
    assert!(tmp.path().join("mail/sendlog.db").exists());
}

#[test]
fn outbox_claims_once_keeps_the_envelope_retries_and_prunes() {
    use store::OutboxEnvelope;

    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let raw = store.blobs().put(b"Message-ID: <e@x>\r\n\r\nhi").unwrap();
    let blob = raw.id();
    let envelope = OutboxEnvelope {
        from: "me@example.org".into(),
        to: vec!["anna@example.com".into(), "hidden@example.net".into()],
    };
    let stored = envelope.clone();
    let id = store
        .write_blocking(move |w| w.outbox_enqueue(None, &raw, "e@x", 0, Some(&stored)))
        .unwrap();
    let item = store.reader().unwrap().outbox_item(id).unwrap().unwrap();
    assert_eq!(item.envelope.as_ref(), Some(&envelope));
    assert!(!format!("{item:?}").contains("anna"));

    // Only one sender gets it.
    assert!(store.write_blocking(move |w| w.outbox_claim(id)).unwrap());
    assert!(!store.write_blocking(move |w| w.outbox_claim(id)).unwrap());
    let reader = store.reader().unwrap();
    assert_eq!(
        reader.outbox_in_state(OutboxState::Sending).unwrap().len(),
        1
    );
    assert_eq!(reader.outbox_item(id).unwrap().unwrap().attempts, 1);
    drop(reader);

    // A temporary failure queues it again for later.
    store
        .write_blocking(move |w| w.outbox_retry_later(id, 500, "connection refused"))
        .unwrap();
    let item = store.reader().unwrap().outbox_item(id).unwrap().unwrap();
    assert_eq!(item.state, OutboxState::Queued);
    assert_eq!(item.send_at, 500);
    assert_eq!(item.last_error.as_deref(), Some("connection refused"));
    assert!(store.reader().unwrap().outbox_due(499).unwrap().is_empty());

    // Sent, but the Sent copy's APPEND still needs the blob: kept.
    store
        .write_blocking(move |w| {
            w.outbox_mark_sent(id)?;
            let account = w.create_account(&store::NewAccount {
                kind: AccountKind::Imap,
                name: "a".into(),
                config_json: "{}".into(),
                ordinal: 0,
            })?;
            w.enqueue_op(
                account,
                "append",
                &format!(r#"{{"v":1,"op":"append","blob":"{}"}}"#, blob.to_hex()),
                "user",
            )?;
            Ok(())
        })
        .unwrap();
    let pruned = store
        .write_blocking(|w| w.outbox_prune_sent(i64::MAX))
        .unwrap();
    assert_eq!(pruned, 0);
    store
        .write_blocking(move |w| {
            let account = w.accounts()?[0].id;
            for op in w.pending_ops(account)? {
                w.op_done(op.id)?;
            }
            Ok(())
        })
        .unwrap();
    let pruned = store
        .write_blocking(|w| w.outbox_prune_sent(i64::MAX))
        .unwrap();
    assert_eq!(pruned, 1);
    assert!(store.reader().unwrap().outbox().unwrap().is_empty());
}

#[test]
fn drafts_round_trip() {
    use mazemail_types::compose::{Draft, DraftAttachment, DraftOrigin};
    use mazemail_types::{Address, MessageId};

    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let mut events = store.subscribe();
    let draft = Draft {
        identity: None,
        to: vec![Address::new(Some("Anna"), "anna@example.com")],
        cc: vec![Address::new(None::<String>, "c@example.com")],
        bcc: vec![Address::new(None::<String>, "b@example.com")],
        subject: "Re: Plans".into(),
        body_markdown: "Hi **Anna**".into(),
        in_reply_to: Some("orig@example.com".into()),
        references: vec!["root@example.com".into(), "orig@example.com".into()],
        attachments: vec![DraftAttachment {
            path: "/tmp/a.pdf".into(),
            name: "a.pdf".into(),
            size: 3,
        }],
        origin: Some(DraftOrigin::ReplyAll(MessageId(7))),
        name: "brave-otter".into(),
        support_maze: true,
    };
    let saved = draft.clone();
    let id = store
        .write_blocking(move |w| w.create_draft(&saved))
        .unwrap();
    assert!(events.blocking_recv().unwrap().drafts);
    let stored = store.reader().unwrap().draft(id).unwrap().unwrap();
    assert_eq!(stored.draft, draft);

    let mut edited = draft;
    edited.subject = "Re: Plans (v2)".into();
    edited.origin = None;
    // "Add “Sent with Maze”" unchecked: kept with the draft.
    edited.support_maze = false;
    let saved = edited.clone();
    store
        .write_blocking(move |w| w.update_draft(id, &saved))
        .unwrap();
    let all = store.reader().unwrap().drafts().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].draft, edited);

    store.write_blocking(move |w| w.delete_draft(id)).unwrap();
    assert!(store.reader().unwrap().draft(id).unwrap().is_none());
    assert!(store.write_blocking(move |w| w.delete_draft(id)).is_err());
}

#[test]
fn tokens_store_only_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let secret = "mazemail_s_0123456789abcdef0123456789abcdef";
    let token = NewApiToken {
        hash: token_hash(secret),
        profile: "read-only".to_owned(),
        scope_json: "{}".to_owned(),
        expires: Some(i64::MAX),
        label: Some("claude".to_owned()),
    };
    let id = store
        .write_blocking(move |w| w.create_api_token(&token))
        .unwrap();
    let reader = store.reader().unwrap();
    let found = reader.api_token_by_secret(secret).unwrap().unwrap();
    assert_eq!(found.id, id);
    assert!(found.is_valid_at(0));
    assert!(
        reader
            .api_token_by_secret("mazemail_s_wrong")
            .unwrap()
            .is_none()
    );
    drop(reader);
    store
        .write_blocking(move |w| w.revoke_api_token(id))
        .unwrap();
    let found = store
        .reader()
        .unwrap()
        .api_token_by_secret(secret)
        .unwrap()
        .unwrap();
    assert!(!found.is_valid_at(0));
}

#[test]
fn tokens_can_be_found_by_precomputed_hash_and_id() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    // A caller-side keyed hash: the store treats it as opaque bytes.
    let hash = [7u8; 32];
    let token = NewApiToken {
        hash,
        profile: "triage".to_owned(),
        scope_json: r#"{"id":"tk_abc"}"#.to_owned(),
        expires: None,
        label: None,
    };
    let id = store
        .write_blocking(move |w| w.create_api_token(&token))
        .unwrap();
    let reader = store.reader().unwrap();
    let found = reader.api_token_by_hash(&hash).unwrap().unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.profile, "triage");
    assert_eq!(reader.api_token(id).unwrap().unwrap(), found);
    assert!(reader.api_token_by_hash(&[8u8; 32]).unwrap().is_none());
    assert!(reader.api_token(TokenId(id.0 + 1)).unwrap().is_none());
}

#[test]
fn proposals_and_audit_log() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let items = vec![
        NewProposalItem {
            command_json: r#"{"archive":["m_a"]}"#.to_owned(),
            reason: Some("newsletter".to_owned()),
        },
        NewProposalItem {
            command_json: r#"{"archive":["m_b"]}"#.to_owned(),
            reason: None,
        },
    ];
    let id = store
        .write_blocking(move |w| w.create_proposal(None, "Archive newsletters", &items))
        .unwrap();
    let proposal = store.reader().unwrap().proposal(id).unwrap().unwrap();
    assert_eq!(proposal.items.len(), 2);
    let (a, b) = (proposal.items[0].id, proposal.items[1].id);
    store
        .write_blocking(move |w| {
            w.set_proposal_item_state(a, ProposalItemState::Approved)?;
            w.set_proposal_item_state(b, ProposalItemState::Rejected)?;
            w.set_proposal_state(id, ProposalState::Partial)?;
            w.audit("user", "proposal.decide", Some(r#"{"proposal":1}"#))?;
            w.audit("agent:7", "search", None)?;
            w.audit("agent:8", "search", None)
        })
        .unwrap();
    let reader = store.reader().unwrap();
    let proposal = reader.proposal(id).unwrap().unwrap();
    assert_eq!(proposal.state, ProposalState::Partial);
    assert!(proposal.decided.is_some());
    assert_eq!(
        reader
            .proposals(Some(ProposalState::Pending))
            .unwrap()
            .len(),
        0
    );
    let agents = reader
        .audit_log(&AuditQuery {
            origin: Some("agent:".to_owned()),
            ..AuditQuery::default()
        })
        .unwrap();
    assert_eq!(
        agents.iter().map(|e| e.origin.as_str()).collect::<Vec<_>>(),
        ["agent:8", "agent:7"]
    );
    let page = reader
        .audit_log(&AuditQuery {
            limit: 1,
            before_id: Some(agents[0].id),
            ..AuditQuery::default()
        })
        .unwrap();
    assert_eq!(page[0].origin, "agent:7");
    assert!(
        store
            .write_blocking(|w| w.create_proposal(None, "empty", &[]))
            .is_err()
    );
}

#[test]
fn settings_rows_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let ids = store
        .write_blocking(move |w| {
            let smtp = w.create_smtp_server(&SmtpServerInput {
                name: Some("Work SMTP".to_owned()),
                host: "smtp.example.com".to_owned(),
                port: Some(465),
                security: SmtpSecurity::Tls,
                auth: SmtpAuth::Plain,
                username: Some("anna".to_owned()),
                saves_sent: SavesSent::Auto,
            })?;
            let signature = w.create_signature(&SignatureInput {
                name: "Work".to_owned(),
                body_html: Some("<p>Anna</p>".to_owned()),
                body_text: Some("Anna".to_owned()),
                ordinal: 0,
            })?;
            let input = IdentityInput {
                email: "anna@example.com".to_owned(),
                display_name: Some("Anna".to_owned()),
                signature: Some(signature),
                smtp: Some(smtp),
                sent_folder: Some(f.sent),
                is_default: true,
                ..IdentityInput::default()
            };
            let first = w.create_identity(f.account, &input)?;
            let second = w.create_identity(
                f.account,
                &IdentityInput {
                    email: "info@example.com".to_owned(),
                    ..input
                },
            )?;
            w.add_identity_rule(second, IdentityRuleKind::Domain, "*@customer.example", 0)?;
            let rule = w.create_rule(&RuleInput {
                account: Some(f.account),
                name: "Newsletters".to_owned(),
                ordinal: 0,
                enabled: true,
                trigger: RuleTrigger::Arrival,
                condition_json: "{}".to_owned(),
                actions_json: "[]".to_owned(),
            })?;
            let search = w.create_saved_search(
                "Unread from Anna",
                "from:anna is:unread",
                Some(&[f.inbox]),
                0,
            )?;
            w.bump_contact("Anna@Example.com", Some("Anna"), 10)?;
            w.bump_contact("anna@example.com", None, 20)?;
            Ok((smtp, signature, first, second, rule, search))
        })
        .unwrap();
    let (smtp, signature, first, second, _rule, _search) = ids;
    let reader = store.reader().unwrap();
    let identities = reader.identities(Some(f.account)).unwrap();
    assert_eq!(identities.len(), 2);
    // Only the newest default stays default.
    assert!(!reader.identity(first).unwrap().unwrap().input.is_default);
    assert!(reader.identity(second).unwrap().unwrap().input.is_default);
    assert_eq!(
        reader.identity_rules(second).unwrap()[0].kind,
        IdentityRuleKind::Domain
    );
    assert_eq!(reader.smtp_server(smtp).unwrap().unwrap().port, Some(465));
    assert_eq!(
        reader
            .signature(signature)
            .unwrap()
            .unwrap()
            .body_text
            .as_deref(),
        Some("Anna")
    );
    assert_eq!(reader.rules(Some(f.account)).unwrap().len(), 1);
    assert_eq!(
        reader.saved_searches().unwrap()[0].folders,
        Some(vec![f.inbox])
    );
    let contacts = reader.search_contacts("ann", 10).unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(
        (contacts[0].frequency, contacts[0].last_used),
        (2, Some(20))
    );
    assert!(
        reader
            .contact_by_email("ANNA@example.com")
            .unwrap()
            .is_some()
    );
    let tree = reader.folder_tree(Some(f.account)).unwrap();
    assert_eq!(tree.len(), 3);
    assert_eq!(
        reader.folders_with_role(FolderRole::Sent, None).unwrap()[0].id,
        f.sent
    );
    drop(reader);

    // Deleting things that identities point at clears the references.
    store
        .write_blocking(move |w| {
            w.delete_signature(signature)?;
            w.delete_smtp_server(smtp)?;
            w.delete_folder(f.sent)
        })
        .unwrap();
    let identity = store.reader().unwrap().identity(second).unwrap().unwrap();
    assert_eq!(
        (
            identity.input.signature,
            identity.input.smtp,
            identity.input.sent_folder
        ),
        (None, None, None)
    );

    // Deleting the account removes everything that belongs to it.
    let m = message(f.account, f.inbox, 1, 1);
    store.write_blocking(move |w| w.upsert_message(&m)).unwrap();
    store
        .write_blocking(move |w| w.delete_account(f.account))
        .unwrap();
    let reader = store.reader().unwrap();
    assert!(reader.accounts().unwrap().is_empty());
    assert!(reader.folders(None).unwrap().is_empty());
    assert!(reader.identities(None).unwrap().is_empty());
}

#[test]
fn folder_tree_and_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let (parent, child) = store
        .write_blocking(move |w| {
            let parent =
                w.create_folder(&store::NewFolder::new(f.account, "Projects", "Projects"))?;
            let mut child = store::NewFolder::new(f.account, "Projects/Maze", "Maze");
            child.parent = Some(parent);
            Ok((parent, w.create_folder(&child)?))
        })
        .unwrap();
    let tree = store
        .reader()
        .unwrap()
        .folder_tree(Some(f.account))
        .unwrap();
    let node = tree.iter().find(|n| n.folder.id == parent).unwrap();
    assert_eq!(node.children[0].folder.id, child);

    // A folder can't move into its own subtree.
    let patch = store::FolderPatch {
        parent: Some(Some(child)),
        ..Default::default()
    };
    assert!(
        store
            .write_blocking(move |w| w.update_folder(parent, &patch))
            .is_err()
    );
    let patch = store::FolderPatch {
        favorite: Some(true),
        display_name: Some("Work projects".into()),
        ..Default::default()
    };
    store
        .write_blocking(move |w| w.update_folder(parent, &patch))
        .unwrap();
    let folder = store.reader().unwrap().folder(parent).unwrap().unwrap();
    assert!(folder.favorite);
    assert_eq!(folder.display_name, "Work projects");
    assert_eq!(folder.remote_name.as_deref(), Some("Projects"));

    // Deleting the parent removes the subtree.
    store
        .write_blocking(move |w| w.delete_folder(parent))
        .unwrap();
    assert!(store.reader().unwrap().folder(child).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_async_writes_are_batched_and_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    // `write_blocking` must not run on a runtime thread.
    let setup = store.clone();
    let f = tokio::task::spawn_blocking(move || common::fixture(&setup, AccountKind::Imap))
        .await
        .unwrap();
    let mut events = store.subscribe();

    let mut tasks = Vec::new();
    for i in 0..300u32 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            let m = message(f.account, f.inbox, i + 1, i64::from(i));
            store.write(move |w| w.upsert_message(&m)).await
        }));
    }
    let mut messages = Vec::new();
    for task in tasks {
        messages.push(task.await.unwrap().unwrap().message);
    }
    // One failing write in the middle of others doesn't affect them.
    let bad = store
        .write(move |w| w.set_location_flags(mazemail_types::MessageId(-1), f.inbox, Flags::SEEN));
    let good = store.write(move |w| w.set_flags(&messages[..10], Flags::SEEN, Flags::empty()));
    let (bad, good) = tokio::join!(bad, good);
    assert!(matches!(bad, Err(store::Error::NotFound { .. })));
    good.unwrap();

    let inbox = store.reader().unwrap().folder(f.inbox).unwrap().unwrap();
    assert_eq!((inbox.total, inbox.unread), (300, 290));

    // Change sets: fewer commits than jobs is likely but not guaranteed; the union must
    // cover every message and the inbox.
    let mut seen_messages = std::collections::BTreeSet::new();
    let mut folders = std::collections::BTreeSet::new();
    while let Ok(change) = events.try_recv() {
        seen_messages.extend(change.messages.iter().copied());
        folders.extend(change.folders.iter().copied());
    }
    assert_eq!(seen_messages.len(), 300);
    assert!(folders.contains(&f.inbox));
    store.close().await.unwrap();
}

#[test]
fn a_panicking_job_is_rolled_back_and_the_writer_survives() {
    let tmp = tempfile::tempdir().unwrap();
    let store = common::open(tmp.path());
    let f = common::fixture(&store, AccountKind::Imap);
    let m = message(f.account, f.inbox, 1, 1);
    let result = store.write_blocking(move |w| {
        w.upsert_message(&m)?;
        if m.date > 0 {
            panic!("bug in a write job");
        }
        Ok(())
    });
    assert!(
        matches!(result, Err(store::Error::Invalid(_))),
        "{result:?}"
    );
    assert_eq!(
        store
            .reader()
            .unwrap()
            .folder(f.inbox)
            .unwrap()
            .unwrap()
            .total,
        0
    );
    let m = message(f.account, f.inbox, 2, 2);
    store.write_blocking(move |w| w.upsert_message(&m)).unwrap();
    assert_eq!(
        store
            .reader()
            .unwrap()
            .folder(f.inbox)
            .unwrap()
            .unwrap()
            .total,
        1
    );
}

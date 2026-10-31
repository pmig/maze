//! The local API services over a real core: scope enforcement, execute vs. propose,
//! tokens in the store, the audit log, the AI policy, import and export, and one
//! round trip through the socket server.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use auth::MemorySecretStore;
use imap::fake::FakeServer;
use import::maildir::MaildirWriter;
use local_api::registry::args::{
    ArchiveArgs, ExportArgs, ExportFormat, FoldersArgs, ImportArgs, SearchArgs, ShowArgs,
    ShowFormat,
};
use local_api::{
    AccountPolicy, ApprovalRequest, Approver, AuditEvent, AuditOutcome, AuditSink, Command,
    CommandError, CommandHandler, CreatedBy, Decision, EffectiveScope, ErrorCode, ExecutionMode,
    Grant, LocalApiClient, LocalApiServer, PeerInfo, ProcessInfo, Profile, RequestContext,
    ServerConfig, TokenRecord, TokenScope, TokenStore, mint,
};
use mazemail_core::api::{self, StoreAuditSink, StoreTokenStore, TokenAuthorizer};
use mazemail_core::{CoreHandle, MailCommand, Origin, SyncEngineConfig};
use mazemail_types::{AccountId, Flags, FolderId, MessageId, Sensitive, ThreadId};
use serde_json::{Value, json};
use store::{AuditQuery, MessageScope, ProposalState, Queries};
use time::OffsetDateTime;
use vault::MemoryKeyStore;

use crate::accounts::{FakeConnector, PASSWORD, input, timing, wait_for};
use crate::common::{self, Fixture, Place};

struct ApiCore {
    core: CoreHandle,
    authorizer: TokenAuthorizer,
    fixture: Fixture,
    work_mail: MessageId,
    work_thread: ThreadId,
    gmail_mail: MessageId,
    projects_mail: MessageId,
    dir: tempfile::TempDir,
}

/// A core with the token authorizer, a settings file sharing both fixture accounts
/// with agents, and one message in Work/INBOX, Work/Projects and Gmail/INBOX.
async fn open() -> ApiCore {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.json");
    std::fs::write(
        &settings,
        r#"{ "agent": { "accounts": { "Work": "any", "Gmail": "any" } } }"#,
    )
    .unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let authorizer = TokenAuthorizer::new();
    let config = common::config(&dir.path().join("data"), &keys)
        .with_settings_file(&settings)
        .with_authorizer(Arc::new(authorizer.clone()));
    let core = CoreHandle::open(config).await.unwrap();
    let fixture = common::fixture(&core).await;
    let raw = |n: u32, from: &str, subject: &str| {
        common::raw_message(
            &format!("api{n}@example.com"),
            from,
            "me@work.example",
            subject,
            &format!("Mon, {n} Oct 2025 10:00:00 +0000"),
            "Quarterly numbers are attached as discussed.",
        )
    };
    let (work_mail, work_thread) = common::ingest(
        &core,
        Place::new(fixture.work, fixture.inbox, 1),
        raw(6, "Anna <anna@example.com>", "Offsite plan"),
    )
    .await;
    let (projects_mail, _) = common::ingest(
        &core,
        Place::new(fixture.work, fixture.projects, 2),
        raw(7, "Ben <ben@example.com>", "Project kickoff"),
    )
    .await;
    let (gmail_mail, _) = common::ingest(
        &core,
        Place::new(fixture.gmail, fixture.g_inbox, 3),
        raw(8, "Shop <receipts@shop.example>", "Your receipt"),
    )
    .await;
    core.commit_index().await.unwrap();
    ApiCore {
        core,
        authorizer,
        fixture,
        work_mail,
        work_thread,
        gmail_mail,
        projects_mail,
        dir,
    }
}

fn peer() -> Arc<PeerInfo> {
    Arc::new(PeerInfo {
        uid: local_api::current_uid(),
        gid: 0,
        process: ProcessInfo {
            pid: Some(4242),
            exe: Some("/usr/local/bin/mazemail".into()),
            parent: None,
        },
    })
}

/// Mints a token with `profile` and `scope` and stores it, as `login` would.
async fn token(t: &ApiCore, profile: Profile, scope: TokenScope) -> TokenRecord {
    let grant = Grant {
        profile,
        scope,
        lifetime: Some(Duration::from_secs(3600)),
        allow_send: false,
    };
    let now =
        OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp()).unwrap();
    let (_, record) = mint(&grant, Some("test".into()), CreatedBy::User, now).unwrap();
    StoreTokenStore::new(t.core.clone(), Arc::clone(t.authorizer.cache()))
        .insert(record.clone())
        .await
        .unwrap();
    record
}

fn handler(t: &ApiCore) -> api::CoreCommandHandler {
    api::CoreCommandHandler::new(t.core.clone(), Arc::clone(t.authorizer.cache()))
}

fn ctx(t: &ApiCore, record: &TokenRecord, mode: ExecutionMode) -> RequestContext {
    let allowed = api::SettingsAccountPolicy::new(t.core.clone()).agent_accounts();
    let scope = EffectiveScope::resolve(&record.scope, &allowed);
    RequestContext::new(1, peer(), Some(record.clone()), scope, mode)
}

fn accounts(ids: &[AccountId]) -> TokenScope {
    TokenScope {
        accounts: Some(ids.iter().copied().collect()),
        folders: None,
    }
}

async fn run(t: &ApiCore, ctx: &RequestContext, command: Command) -> Result<Value, CommandError> {
    handler(t)
        .handle(ctx, command)
        .await
        .map(|output| output.data.into_inner())
}

fn search(query: &str) -> Command {
    Command::Search(SearchArgs {
        query: Sensitive::new(query.to_owned()),
        limit: 50,
        ids: false,
    })
}

fn show(id: impl std::fmt::Display, format: ShowFormat) -> Command {
    Command::Show(ShowArgs {
        id: id.to_string(),
        format,
    })
}

fn ids(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn account_scope_hides_other_accounts() {
    let t = open().await;
    let record = token(&t, Profile::ReadOnly, accounts(&[t.fixture.work])).await;
    let ctx = ctx(&t, &record, ExecutionMode::Execute);

    let hits = run(&t, &ctx, search("example")).await.unwrap();
    assert_eq!(
        ids(&hits),
        BTreeSet::from([t.work_mail.to_string(), t.projects_mail.to_string()])
    );

    // Out of scope looks exactly like a missing id.
    for command in [
        show(t.gmail_mail, ShowFormat::Md),
        show(MessageId(999_999), ShowFormat::Md),
    ] {
        let error = run(&t, &ctx, command).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::NotFound, "{error}");
    }
    let error = run(
        &t,
        &ctx,
        Command::Archive(ArchiveArgs {
            ids: vec![t.gmail_mail.to_string()],
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    let error = run(
        &t,
        &ctx,
        Command::Folders(FoldersArgs {
            account: Some(t.fixture.gmail),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);

    let folders = run(&t, &ctx, Command::Folders(FoldersArgs::default()))
        .await
        .unwrap();
    let work = t.fixture.work.to_string();
    assert!(
        folders
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["account"] == work.as_str()),
        "{folders}"
    );

    // In scope: the message renders as Markdown, the thread too.
    let shown = run(&t, &ctx, show(t.work_mail, ShowFormat::Md))
        .await
        .unwrap();
    let content = shown["content"].as_str().unwrap();
    assert!(content.starts_with("## Offsite plan\n"), "{content}");
    assert!(
        content.contains("**From:** Anna <anna@example.com>"),
        "{content}"
    );
    assert!(content.contains("Quarterly numbers"), "{content}");
    let thread = run(&t, &ctx, show(t.work_thread, ShowFormat::Text))
        .await
        .unwrap();
    assert!(
        thread["content"]
            .as_str()
            .unwrap()
            .starts_with("Offsite plan\n")
    );
    let raw = run(&t, &ctx, show(t.work_mail, ShowFormat::Raw))
        .await
        .unwrap();
    assert!(
        raw["content"]
            .as_str()
            .unwrap()
            .contains("Message-ID: <api6@example.com>")
    );
}

#[tokio::test]
async fn folder_scope_and_ai_policy_narrow_further() {
    let t = open().await;
    let scope = TokenScope {
        accounts: None,
        folders: Some(BTreeSet::from([t.fixture.inbox, t.fixture.g_inbox])),
    };
    let record = token(&t, Profile::ReadOnly, scope).await;
    let ctx = ctx(&t, &record, ExecutionMode::Execute);
    let hits = run(&t, &ctx, search("example")).await.unwrap();
    assert_eq!(
        ids(&hits),
        BTreeSet::from([t.work_mail.to_string(), t.gmail_mail.to_string()])
    );
    let error = run(&t, &ctx, show(t.projects_mail, ShowFormat::Md))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);

    // Turning agents off for Gmail removes it from every token at once.
    std::fs::write(
        t.dir.path().join("settings.json"),
        r#"{ "agent": { "accounts": { "Work": "any", "Gmail": "off" } } }"#,
    )
    .unwrap();
    let policy = api::SettingsAccountPolicy::new(t.core.clone());
    tokio::time::timeout(Duration::from_secs(10), async {
        while policy.agent_accounts().contains(&t.fixture.gmail) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the settings watcher picks up the change");
    let ctx = crate::api::ctx(&t, &record, ExecutionMode::Execute);
    let hits = run(&t, &ctx, search("example")).await.unwrap();
    assert_eq!(ids(&hits), BTreeSet::from([t.work_mail.to_string()]));
}

#[tokio::test]
async fn triage_proposes_and_writer_applies() {
    let t = open().await;
    let archive = || {
        Command::Archive(ArchiveArgs {
            ids: vec![t.work_thread.to_string()],
        })
    };

    // Triage: the dispatcher says "propose"; nothing moves.
    let triage = token(&t, Profile::Triage, TokenScope::all()).await;
    let out = run(&t, &ctx(&t, &triage, ExecutionMode::Propose), archive())
        .await
        .unwrap();
    assert_eq!(out["outcome"], "proposed");
    assert!(out["proposal"].as_str().unwrap().starts_with("p_"));
    assert_eq!(out["affected"], json!([t.work_mail.to_string()]));
    let reader = t.core.store().reader().unwrap();
    let proposals = reader.proposals(Some(ProposalState::Pending)).unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].title, "Archive 1 message");
    let location = reader.message(t.work_mail).unwrap().unwrap().locations;
    assert_eq!(location[0].folder, t.fixture.inbox);

    // Even if a triage token reached `apply` directly, core proposes instead.
    let triage_id = t.authorizer.cache().store_id(&triage.id).unwrap();
    let receipt = t
        .core
        .apply(
            MailCommand::Archive {
                ids: vec![t.work_mail],
            },
            Origin::Agent { token: triage_id },
        )
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome,
        mazemail_core::CommandOutcome::Proposed(_)
    ));

    // Writer: applied.
    let writer = token(&t, Profile::Writer, TokenScope::all()).await;
    let out = run(&t, &ctx(&t, &writer, ExecutionMode::Execute), archive())
        .await
        .unwrap();
    assert_eq!(out["outcome"], "applied");
    let folders: Vec<FolderId> = t
        .core
        .store()
        .reader()
        .unwrap()
        .message(t.work_mail)
        .unwrap()
        .unwrap()
        .locations
        .iter()
        .map(|l| l.folder)
        .collect();
    assert!(!folders.contains(&t.fixture.inbox), "{folders:?}");

    // A read-only token never changes mail, whatever reaches `apply`.
    let reader_token = token(&t, Profile::ReadOnly, TokenScope::all()).await;
    let id = t.authorizer.cache().store_id(&reader_token.id).unwrap();
    let error = t
        .core
        .apply(
            MailCommand::Archive {
                ids: vec![t.projects_mail],
            },
            Origin::Agent { token: id },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        mazemail_core::Error::PermissionDenied { .. }
    ));
}

#[tokio::test]
async fn tokens_live_in_the_store() {
    let t = open().await;
    let tokens = StoreTokenStore::new(t.core.clone(), Arc::clone(t.authorizer.cache()));
    let record = token(&t, Profile::Triage, accounts(&[t.fixture.work])).await;

    let found = tokens.find_by_hash(&record.hash).await.unwrap().unwrap();
    assert_eq!(found, record);
    assert_eq!(tokens.list().await.unwrap(), vec![record.clone()]);

    // A fresh cache (another process) learns the token from the store.
    let fresh = StoreTokenStore::new(t.core.clone(), Arc::default());
    assert_eq!(
        fresh.find_by_hash(&record.hash).await.unwrap(),
        Some(record.clone())
    );

    assert!(
        tokens
            .revoke(&record.id, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    assert!(
        !tokens
            .revoke(&record.id, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    let revoked = tokens.find_by_hash(&record.hash).await.unwrap().unwrap();
    assert!(revoked.revoked_at.is_some());
    // The authorizer refuses it right away.
    let id = t.authorizer.cache().store_id(&record.id).unwrap();
    let error = t
        .core
        .apply(
            MailCommand::Archive {
                ids: vec![t.work_mail],
            },
            Origin::Agent { token: id },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        mazemail_core::Error::PermissionDenied { .. }
    ));
    // No secret or hash shows up in the stored scope.
    let row = t
        .core
        .store()
        .reader()
        .unwrap()
        .api_token(id)
        .unwrap()
        .unwrap();
    assert!(!row.scope_json.contains(&record.hash.to_hex()));
    assert_eq!(row.profile, "triage");
}

#[tokio::test]
async fn audit_events_land_in_the_audit_log() {
    let t = open().await;
    let record = token(&t, Profile::ReadOnly, TokenScope::all()).await;
    let sink = StoreAuditSink::new(&t.core, Arc::clone(t.authorizer.cache()));
    sink.record(AuditEvent {
        at: OffsetDateTime::now_utc(),
        request_id: 7,
        token: Some(record.id.clone()),
        pid: Some(4242),
        exe: Some("/usr/local/bin/mazemail".into()),
        cmd: "search".into(),
        outcome: AuditOutcome::Executed,
        approved: false,
        affected: vec![t.work_mail.to_string()],
    });
    sink.record(AuditEvent {
        at: OffsetDateTime::now_utc(),
        request_id: 8,
        token: None,
        pid: Some(4242),
        exe: None,
        cmd: "show".into(),
        outcome: AuditOutcome::Rejected {
            code: ErrorCode::NotSignedIn,
        },
        approved: false,
        affected: Vec::new(),
    });
    let store_id = t.authorizer.cache().store_id(&record.id).unwrap();
    let entries = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let entries = t
                .core
                .store()
                .reader()
                .unwrap()
                .audit_log(&AuditQuery {
                    limit: 10,
                    ..AuditQuery::default()
                })
                .unwrap();
            if entries
                .iter()
                .filter(|e| e.action.starts_with("api:"))
                .count()
                == 2
            {
                return entries;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the audit writer stores both events");
    let search = entries.iter().find(|e| e.action == "api:search").unwrap();
    assert_eq!(search.origin, format!("agent:{}", store_id.0));
    let detail: Value = serde_json::from_str(search.detail_json.as_deref().unwrap()).unwrap();
    assert_eq!(detail["request"], 7);
    assert_eq!(detail["token"], record.id.as_str());
    assert_eq!(detail["outcome"]["kind"], "executed");
    assert_eq!(detail["affected"], json!([t.work_mail.to_string()]));
    let show = entries.iter().find(|e| e.action == "api:show").unwrap();
    assert_eq!(show.origin, "api");
}

fn mbox() -> String {
    let mut text = String::new();
    for n in 1..=3 {
        text.push_str(&format!(
            "From importer@example.org Mon Oct 13 09:0{n}:00 2025\n\
             Message-ID: <imported{n}@example.org>\n\
             Date: Mon, 13 Oct 2025 09:0{n}:00 +0000\n\
             From: Importer <importer@example.org>\n\
             To: me@work.example\n\
             Subject: Archived note {n}\n\
             \n\
             Zanzibar itinerary number {n}.\n\n"
        ));
    }
    text
}

#[tokio::test]
async fn import_queues_uploads_and_export_writes_mbox() {
    let t = open().await;
    let writer = token(&t, Profile::Writer, TokenScope::all()).await;
    let ctx = ctx(&t, &writer, ExecutionMode::Execute);
    let source = t.dir.path().join("old.mbox");
    std::fs::write(&source, mbox()).unwrap();
    let import = |account: Option<AccountId>, folder: Option<&str>| {
        Command::Import(ImportArgs {
            path: source.clone(),
            format: None,
            account,
            folder: folder.map(str::to_owned),
        })
    };

    // Two accounts: which one has to be said.
    let error = run(&t, &ctx, import(None, None)).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);
    assert!(error.message.contains("--account"), "{}", error.message);

    // Into a new folder named after the file: created on the server, then one upload
    // per message, queued for the sync engine.
    let out = run(&t, &ctx, import(Some(t.fixture.work), None))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({ "messages": 3, "duplicates": 0, "folders": 1, "accounts": 0, "skipped": [] })
    );
    let reader = t.core.store().reader().unwrap();
    let folder = reader
        .folders(Some(t.fixture.work))
        .unwrap()
        .into_iter()
        .find(|f| f.display_name == "old")
        .unwrap();
    assert_eq!(folder.remote_name.as_deref(), Some("old"));
    let kinds: Vec<String> = reader
        .pending_ops(t.fixture.work)
        .unwrap()
        .into_iter()
        .map(|op| op.kind)
        .collect();
    assert_eq!(kinds, ["create_folder", "append", "append", "append"]);
    drop(reader);

    // Idempotent; an existing folder is found by its path.
    let again = run(&t, &ctx, import(Some(t.fixture.work), Some("old")))
        .await
        .unwrap();
    assert_eq!(
        (&again["messages"], &again["duplicates"]),
        (&json!(0), &json!(3))
    );
    assert_eq!(again["folders"], 0);
    // Nested folders are created level by level.
    let nested = run(&t, &ctx, import(Some(t.fixture.work), Some("Imports/2025")))
        .await
        .unwrap();
    assert_eq!(
        (&nested["messages"], &nested["folders"]),
        (&json!(3), &json!(2))
    );
    let error = run(&t, &ctx, import(Some(t.fixture.work), Some("Archive//x")))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);

    // Export of a query: mbox, oldest first, mode 0600, never overwriting.
    let target = t.dir.path().join("export.mbox");
    let out = run(
        &t,
        &ctx_all(&t, &writer),
        Command::Export(ExportArgs {
            query: Sensitive::new("quarterly".into()),
            to: target.clone(),
            format: ExportFormat::Mbox,
        }),
    )
    .await
    .unwrap();
    assert_eq!(out["messages"], 3);
    let text = std::fs::read_to_string(&target).unwrap();
    assert_eq!(text.matches("\nSubject: ").count(), 3);
    assert!(text.find("Offsite plan").unwrap() < text.find("Your receipt").unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let error = run(
        &t,
        &ctx_all(&t, &writer),
        Command::Export(ExportArgs {
            query: Sensitive::new("quarterly".into()),
            to: target,
            format: ExportFormat::Mbox,
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidArgs);
}

/// An mbox with Thunderbird's status headers: one read and starred message with a tag,
/// one unread.
const THUNDERBIRD_MBOX: &str = "From - Mon Oct 13 09:00:00 2025\n\
    X-Mozilla-Status: 0005\n\
    X-Mozilla-Keys: projekt\n\
    Message-ID: <tb1@example.org>\n\
    Date: Mon, 13 Oct 2025 09:00:00 +0000\n\
    From: Importer <importer@example.org>\n\
    Subject: Kept for years\n\
    \n\
    The first one.\n\
    \n\
    From - Mon Oct 13 09:01:00 2025\n\
    X-Mozilla-Status: 0000\n\
    Message-ID: <tb2@example.org>\n\
    Date: Mon, 13 Oct 2025 09:01:00 +0000\n\
    From: Importer <importer@example.org>\n\
    Subject: Never read\n\
    \n\
    The second one.\n\
    \n";

#[tokio::test(flavor = "multi_thread")]
async fn imported_mail_is_uploaded_to_the_imap_server_once() {
    let server = FakeServer::new();
    let dir = tempfile::tempdir().unwrap();
    let keys = Arc::new(MemoryKeyStore::new());
    let authorizer = TokenAuthorizer::new();
    let engine = SyncEngineConfig::new(Arc::new(MemorySecretStore::new()))
        .with_connector(Arc::new(FakeConnector {
            server: server.clone(),
            reachable: true,
        }))
        .with_timing(timing());
    let config = common::config(&dir.path().join("data"), &keys)
        .with_sync_engine(engine)
        .with_authorizer(Arc::new(authorizer.clone()));
    let core = CoreHandle::open(config).await.unwrap();
    let account = core.add_imap_account(input(PASSWORD)).await.unwrap().id;
    let folder_named = |name: &str| {
        core.store()
            .reader()
            .unwrap()
            .folders(Some(account))
            .unwrap()
            .into_iter()
            .find(|f| f.remote_name.as_deref() == Some(name))
    };
    wait_for("the folder list", || folder_named("Archive").is_some()).await;

    let grant = Grant {
        profile: Profile::Writer,
        scope: TokenScope::all(),
        lifetime: Some(Duration::from_secs(3600)),
        allow_send: false,
    };
    let now =
        OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp()).unwrap();
    let (_, record) = mint(&grant, None, CreatedBy::User, now).unwrap();
    StoreTokenStore::new(core.clone(), Arc::clone(authorizer.cache()))
        .insert(record.clone())
        .await
        .unwrap();
    let all: BTreeSet<AccountId> = core.accounts().unwrap().iter().map(|a| a.id).collect();
    let ctx = RequestContext::new(
        1,
        peer(),
        Some(record.clone()),
        EffectiveScope::resolve(&record.scope, &all),
        ExecutionMode::Execute,
    );
    let handler = api::CoreCommandHandler::new(core.clone(), Arc::clone(authorizer.cache()));
    let source = dir.path().join("Old Mail.mbox");
    std::fs::write(&source, THUNDERBIRD_MBOX).unwrap();
    let import = |path: &std::path::Path, folder: &str| {
        Command::Import(ImportArgs {
            path: path.to_owned(),
            format: None,
            // The only account: no --account needed.
            account: None,
            folder: Some(folder.to_owned()),
        })
    };

    // Archive exists, 2019 is created; the messages go up with their flags and tags.
    let out = handler
        .handle(&ctx, import(&source, "Archive/2019"))
        .await
        .unwrap()
        .data
        .into_inner();
    assert_eq!((&out["messages"], &out["folders"]), (&json!(2), &json!(1)));
    wait_for("the uploads", || server.messages("Archive/2019").len() == 2).await;
    let mut uploaded = server.messages("Archive/2019");
    uploaded.sort_by_key(|m| m.uid);
    assert_eq!(uploaded[0].flags, Flags::SEEN | Flags::FLAGGED);
    assert_eq!(uploaded[0].keywords, ["projekt"]);
    assert_eq!(uploaded[1].flags, Flags::empty());

    // The folder's sync brings them in like any other mail.
    let synced = || {
        let Some(folder) = folder_named("Archive/2019") else {
            return 0;
        };
        core.store()
            .reader()
            .unwrap()
            .message_ids_in_scope(&MessageScope::Folder(folder.id))
            .unwrap()
            .len()
    };
    wait_for("the uploaded mail locally", || synced() == 2).await;

    // Running it again uploads nothing.
    let again = handler
        .handle(&ctx, import(&source, "Archive/2019"))
        .await
        .unwrap()
        .data
        .into_inner();
    assert_eq!(
        (&again["messages"], &again["duplicates"]),
        (&json!(0), &json!(2))
    );
    assert_eq!(server.messages("Archive/2019").len(), 2);

    // A Maildir with a Maildir++ subfolder: both folders are created.
    let maildir = dir.path().join("Mail");
    let mut writer = MaildirWriter::create(&maildir).unwrap();
    writer
        .write(
            b"Message-ID: <md1@example.org>\r\nSubject: md\r\n\r\nx\r\n",
            Flags::SEEN,
            &[],
        )
        .unwrap();
    writer.finish().unwrap();
    let mut sent = MaildirWriter::create(&maildir.join(".Sent")).unwrap();
    sent.write(
        b"Message-ID: <md2@example.org>\r\nSubject: md2\r\n\r\ny\r\n",
        Flags::SEEN,
        &[],
    )
    .unwrap();
    sent.finish().unwrap();
    let out = handler
        .handle(&ctx, import(&maildir, "Maildir"))
        .await
        .unwrap()
        .data
        .into_inner();
    assert_eq!((&out["messages"], &out["folders"]), (&json!(2), &json!(2)));
    wait_for("the Maildir uploads", || {
        server.messages("Maildir").len() == 1 && server.messages("Maildir/Sent").len() == 1
    })
    .await;
    core.shutdown().await.unwrap();
}

/// A context whose scope includes every account, as the AI policy decides on each
/// request.
fn ctx_all(t: &ApiCore, record: &TokenRecord) -> RequestContext {
    let all: BTreeSet<AccountId> = t.core.accounts().unwrap().iter().map(|a| a.id).collect();
    let scope = EffectiveScope::resolve(&record.scope, &all);
    RequestContext::new(
        2,
        peer(),
        Some(record.clone()),
        scope,
        ExecutionMode::Execute,
    )
}

#[tokio::test]
async fn unbuilt_commands_say_so() {
    let t = open().await;
    let record = token(&t, Profile::Writer, TokenScope::all()).await;
    let error = run(
        &t,
        &ctx(&t, &record, ExecutionMode::Execute),
        Command::Send(local_api::registry::args::SendArgs {
            draft: "d_b".into(),
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error
            .message
            .contains("not available in this version of Maze")
    );
}

/// Approves every login as requested.
struct Approve;

#[async_trait]
impl Approver for Approve {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        Decision::Approve(request.requested_grant())
    }
}

#[tokio::test]
async fn socket_round_trip() {
    let t = open().await;
    let socket_dir = tempfile::Builder::new()
        .prefix("mz")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = settings::SocketPath::InDataDir(socket_dir.path().join("m.sock"));
    let services = api::services(&t.core, &t.authorizer, Arc::new(Approve));
    let server = LocalApiServer::bind_socket(&socket, None, services, ServerConfig::default())
        .await
        .unwrap();
    let path = server.path().to_owned();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));

    let endpoint = local_api::Endpoint::Unix(path.clone());
    let mut client = LocalApiClient::connect(&endpoint).await.unwrap();
    // Without a token: status works, mail doesn't.
    let status = client.call("status", json!({})).await.unwrap();
    assert_eq!(status["signed_in"], false);
    let error = client
        .call("search", json!({ "query": "offsite" }))
        .await
        .unwrap_err();
    assert_eq!(error.exit_code().code(), 6);

    let login = client
        .call("login", json!({ "profile": "triage", "ttl": "1h" }))
        .await
        .unwrap();
    let secret = login["token"].as_str().unwrap().to_owned();
    let mut client = LocalApiClient::connect(&endpoint)
        .await
        .unwrap()
        .with_token(Some(local_api::SessionToken::from_string(secret)));
    let status = client.call("status", json!({})).await.unwrap();
    assert_eq!(status["signed_in"], true);
    assert_eq!(status["app"]["accounts"].as_array().unwrap().len(), 2);
    let hits = client
        .call("search", json!({ "query": "subject:offsite" }))
        .await
        .unwrap();
    assert_eq!(ids(&hits), BTreeSet::from([t.work_mail.to_string()]));
    let archived = client
        .call("archive", json!({ "ids": [t.work_mail.to_string()] }))
        .await
        .unwrap();
    assert_eq!(archived["outcome"], "proposed");
    let listed = client.call("token.list", json!({})).await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let _ = stop.send(());
    serving.await.unwrap();
    assert!(!path.exists(), "the socket is removed on shutdown");

    // Every request was audited (asynchronously), with the token's store id where
    // known.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let entries = t
                .core
                .store()
                .reader()
                .unwrap()
                .audit_log(&AuditQuery {
                    limit: 50,
                    ..AuditQuery::default()
                })
                .unwrap();
            let login = entries.iter().any(|e| e.action == "api:login");
            let archive = entries
                .iter()
                .any(|e| e.action == "api:archive" && e.origin.starts_with("agent:"));
            if login && archive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the requests are audited");
}

#[tokio::test]
async fn account_scope_limits_synced_contacts_to_its_accounts() {
    let t = open().await;
    let reader = t.core.store().reader().unwrap();
    let work = reader.account(t.fixture.work).unwrap().unwrap().uuid;
    let gmail = reader.account(t.fixture.gmail).unwrap().unwrap().uuid;
    drop(reader);
    let (work_id, gmail_id) = (t.fixture.work, t.fixture.gmail);
    t.core
        .store()
        .write(move |w| {
            for (account, uuid, email) in [
                (work_id, work, "colleague@work.example"),
                (gmail_id, gmail, "friend@gmail.example"),
            ] {
                let book = format!("google:{uuid}");
                w.ensure_contact_source(&book, account, "google")?;
                w.upsert_server_contact(
                    &book,
                    &store::ServerContact {
                        resource_name: "people/c1".into(),
                        etag: None,
                        display_name: Some("Someone".into()),
                        emails: vec![email.into()],
                        vcard: "BEGIN:VCARD\r\nEND:VCARD\r\n".into(),
                        photo_url: None,
                    },
                    1,
                )?;
            }
            w.upsert_contact("thunderbird", "imported@example.org", None, None)?;
            w.bump_contact("collected@example.org", None, 1)?;
            Ok(())
        })
        .await
        .unwrap();
    let contacts = |value: Value| -> BTreeSet<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["email"].as_str().unwrap().to_owned())
            .collect()
    };
    let command = || {
        Command::Contacts(local_api::registry::args::ContactsArgs {
            query: Sensitive::new(String::new()),
            limit: 50,
        })
    };
    let record = token(&t, Profile::ReadOnly, accounts(&[t.fixture.work])).await;
    let scoped = run(&t, &ctx(&t, &record, ExecutionMode::Execute), command())
        .await
        .unwrap();
    assert_eq!(
        contacts(scoped),
        BTreeSet::from([
            "colleague@work.example".to_owned(),
            "imported@example.org".to_owned()
        ])
    );
    let record = token(&t, Profile::ReadOnly, TokenScope::default()).await;
    let all = run(&t, &ctx(&t, &record, ExecutionMode::Execute), command())
        .await
        .unwrap();
    assert_eq!(contacts(all).len(), 4);
}

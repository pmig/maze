//! Thunderbird profile: discovery, parsed models (snapshots), dry run and import.

use std::sync::Mutex;

use import::thunderbird::{
    ImportOptions, ProfileLocator, ThunderbirdProfile, import, model::ThunderbirdConfig,
};
use import::{BodyReuse, Phase, ProgressUpdate};
use mazemail_types::{Flags, FolderRole};
use pretty_assertions::assert_eq;
use store::{AccountKind, MessageScope, Queries, RuleTrigger, SmtpAuth, SmtpSecurity};

use crate::common::{Env, redact, thunderbird_copy, vault};

#[test]
fn finds_the_default_profile() {
    let env = Env::new();
    let (root, profile) = thunderbird_copy(&env);
    let found = ProfileLocator::with_roots(vec![root])
        .default_profile()
        .unwrap()
        .unwrap();
    assert_eq!(found.path, profile);
    assert_eq!(found.name, "default-release");
    assert!(matches!(
        ThunderbirdProfile::load(&env.scratch("empty")),
        Err(import::Error::NoProfile(_))
    ));
    env.close();
}

#[test]
fn parsed_models_snapshot() {
    let env = Env::new();
    let (_, profile_dir) = thunderbird_copy(&env);
    let profile = ThunderbirdProfile::load(&profile_dir).unwrap();
    assert_eq!(profile.prefs.errors, 1);

    let config: &ThunderbirdConfig = &profile.config;
    insta::assert_yaml_snapshot!(
        "config",
        redact(serde_json::to_value(config).unwrap(), &profile_dir)
    );
    insta::assert_yaml_snapshot!(
        "virtual_folders",
        redact(
            serde_json::to_value(&profile.virtual_folders).unwrap(),
            &profile_dir
        )
    );
    insta::assert_yaml_snapshot!(
        "filters",
        redact(
            serde_json::to_value(&profile.filters).unwrap(),
            &profile_dir
        )
    );
    insta::assert_yaml_snapshot!(
        "layout",
        redact(serde_json::to_value(&profile.layout).unwrap(), &profile_dir)
    );
    let books: Vec<_> = profile
        .address_books
        .iter()
        .map(|(path, name)| import::thunderbird::abook::read(path, name).unwrap())
        .collect();
    insta::assert_yaml_snapshot!(
        "address_books",
        redact(serde_json::to_value(&books).unwrap(), &profile_dir)
    );
    env.close();
}

#[test]
fn dry_run_report_snapshot() {
    let env = Env::new();
    let (_, profile_dir) = thunderbird_copy(&env);
    let profile = ThunderbirdProfile::load(&profile_dir).unwrap();
    let report = profile.report().unwrap();
    assert_eq!(report.sign_in_again().count(), 1);
    assert_eq!(report.password_prompts().count(), 1);
    insta::assert_yaml_snapshot!(
        "report",
        redact(serde_json::to_value(&report).unwrap(), &profile_dir)
    );
    // The dry run writes nothing.
    assert!(env.store.reader().unwrap().accounts().unwrap().is_empty());
    env.close();
}

#[test]
fn imports_everything_and_reruns_without_duplicates() {
    let env = Env::new();
    let (_, profile_dir) = thunderbird_copy(&env);
    let profile = ThunderbirdProfile::load(&profile_dir).unwrap();
    let reuse = BodyReuse::create(&env.scratch("data").join("reuse.db"), &vault(3)).unwrap();
    let updates = Mutex::new(Vec::<ProgressUpdate>::new());
    let progress = |u: ProgressUpdate| updates.lock().unwrap().push(u);
    let outcome = import(
        &profile,
        &env.ingestor,
        Some(&reuse),
        &ImportOptions::default(),
        &progress,
    )
    .unwrap();

    assert_eq!(outcome.accounts_created, 3);
    assert_eq!(outcome.smtp_servers, 2);
    assert_eq!(outcome.identities, 3);
    assert_eq!(outcome.signatures, 2);
    // `$label1` renamed, `projekt` new; the other defaults are seeded by the store.
    assert_eq!(outcome.tags, 2);
    assert_eq!(outcome.contacts, 4);
    assert_eq!(outcome.rules, 3);
    assert_eq!(outcome.saved_searches, 2);
    assert_eq!(outcome.messages.imported, 4);
    assert_eq!(outcome.messages.deleted, 1);
    assert_eq!(outcome.messages.folders_created, 6);
    assert_eq!(outcome.body_index.unwrap().indexed, 10);
    assert!(outcome.warnings.is_empty(), "{:?}", outcome.warnings);
    assert!(outcome.layout.is_some());
    let phases: Vec<Phase> = updates.lock().unwrap().iter().map(|u| u.phase).collect();
    for phase in [
        Phase::Accounts,
        Phase::Messages,
        Phase::Contacts,
        Phase::BodyIndex,
    ] {
        assert!(phases.contains(&phase), "{phase:?} missing");
    }

    let reader = env.store.reader().unwrap();
    let accounts = reader.accounts().unwrap();
    let kinds: Vec<(AccountKind, &str)> =
        accounts.iter().map(|a| (a.kind, a.name.as_str())).collect();
    assert_eq!(
        kinds,
        vec![
            (AccountKind::Imap, "Work"),
            (AccountKind::Gmail, "anna.example@gmail.com"),
            (AccountKind::Local, "Local Folders"),
        ]
    );
    let work_config: serde_json::Value = serde_json::from_str(&accounts[0].config_json).unwrap();
    assert_eq!(work_config["host"], "imap.example.com");
    assert_eq!(work_config["security"], "tls");
    assert_eq!(work_config["auth"], "password");
    assert_eq!(
        work_config["identity_folders"][0]["sent"],
        "imap://anna%40example.com@imap.example.com/Sent"
    );

    let smtp = reader.smtp_servers().unwrap();
    assert_eq!(smtp.len(), 2);
    let gmail_smtp = smtp.iter().find(|s| s.host == "smtp.gmail.com").unwrap();
    assert_eq!(gmail_smtp.security, SmtpSecurity::Tls);
    assert_eq!(gmail_smtp.auth, SmtpAuth::OAuth2);

    // Local folder tree with roles.
    let local = accounts[2].id;
    let folders = reader.folders(Some(local)).unwrap();
    let role = |name: &str| {
        folders
            .iter()
            .find(|f| f.display_name == name)
            .unwrap()
            .role
    };
    assert_eq!(role("Inbox"), Some(FolderRole::Inbox));
    assert_eq!(role("Trash"), Some(FolderRole::Trash));
    assert_eq!(role("Drafts"), Some(FolderRole::Drafts));
    assert_eq!(role("Archives"), Some(FolderRole::Archive));
    assert_eq!(role("Unsent Messages"), None);
    let folder = |name: &str| folders.iter().find(|f| f.display_name == name).unwrap().id;

    // Messages, flags, keywords, threads.
    let inbox_ids = reader
        .message_ids_in_scope(&MessageScope::Folder(folder("Inbox")))
        .unwrap();
    assert_eq!(inbox_ids.len(), 3);
    let details: Vec<_> = inbox_ids
        .iter()
        .map(|id| reader.message(*id).unwrap().unwrap())
        .collect();
    let by_id = |rfc: &str| {
        details
            .iter()
            .find(|m| m.rfc_message_id.as_deref() == Some(rfc))
            .unwrap()
    };
    let l1 = by_id("l1@local.example");
    let l2 = by_id("l2@local.example");
    assert_eq!(l1.thread, l2.thread);
    assert_eq!(l1.locations[0].flags, Flags::SEEN);
    assert_eq!(
        l2.locations[0].flags,
        Flags::ANSWERED | Flags::FLAGGED | Flags::FORWARDED
    );
    assert_eq!(l2.locations[0].keywords, vec!["$label2", "todo"]);
    let l1_raw = env.store.blobs().read(&l1.blob.unwrap()).unwrap();
    let text = String::from_utf8_lossy(&l1_raw);
    assert!(text.contains("\r\n>>From two levels deep\r\n"), "{text}");
    assert!(!text.contains("X-Mozilla"));
    assert!(!text.contains("\n\n"));
    let archived = reader
        .message_ids_in_scope(&MessageScope::Folder(folder("2024")))
        .unwrap();
    assert_eq!(archived.len(), 1);
    let tags = reader.tags().unwrap();
    assert!(
        tags.iter()
            .any(|t| t.keyword == "projekt" && t.name == "Projekt \"Atlas\"")
    );
    assert!(
        tags.iter()
            .any(|t| t.keyword == "$label1" && t.name == "Wichtig")
    );

    // Identities: signatures, SMTP, catch-all, local folders resolved.
    let identities = reader.identities(None).unwrap();
    assert_eq!(identities.len(), 3);
    let anna = identities
        .iter()
        .find(|i| i.input.email == "anna@example.com")
        .unwrap();
    assert!(anna.input.is_default);
    assert_eq!(anna.input.archive_granularity, 2);
    assert_eq!(
        anna.input.match_patterns_json.as_deref(),
        Some(r#"["*@anna.example","info@example.com"]"#)
    );
    let signature = reader
        .signature(anna.input.signature.unwrap())
        .unwrap()
        .unwrap();
    assert!(signature.body_html.unwrap().contains("<b>Anna Example</b>"));
    let billing = identities
        .iter()
        .find(|i| i.input.email == "billing@example.com")
        .unwrap();
    assert_eq!(billing.input.drafts_folder, Some(folder("Drafts")));
    assert_eq!(billing.input.sent_folder, Some(folder("2024")));
    assert_eq!(billing.input.archive_granularity, 0);
    let file_sig = reader
        .signature(billing.input.signature.unwrap())
        .unwrap()
        .unwrap();
    assert!(
        file_sig
            .body_text
            .unwrap()
            .contains("Billing, Example GmbH")
    );
    assert_eq!(anna.input.smtp, billing.input.smtp);

    // Rules are disabled with the parsed filter and a note.
    let rules = reader.rules(None).unwrap();
    assert_eq!(rules.len(), 3);
    assert!(rules.iter().all(|r| !r.input.enabled));
    let lists = rules
        .iter()
        .find(|r| r.input.name == "Lists to folder")
        .unwrap();
    assert_eq!(lists.input.account, Some(accounts[0].id));
    assert_eq!(lists.input.trigger, RuleTrigger::ArrivalBeforeJunk);
    let condition: serde_json::Value = serde_json::from_str(&lists.input.condition_json).unwrap();
    assert_eq!(condition["terms"][0]["attribute"], "list-id");
    assert!(
        condition["import_note"]
            .as_str()
            .unwrap()
            .contains("disabled")
    );
    let periodic = rules
        .iter()
        .find(|r| r.input.name == "Everything periodic")
        .unwrap();
    assert_eq!(periodic.input.trigger, RuleTrigger::Periodic);
    assert_eq!(periodic.input.account, Some(local));
    let boss = rules
        .iter()
        .find(|r| r.input.name == "Tag the boss")
        .unwrap();
    let actions: serde_json::Value = serde_json::from_str(&boss.input.actions_json).unwrap();
    assert_eq!(actions[1]["supported"], false);

    // Saved searches: local scopes resolve, IMAP scopes cover everything for now.
    let searches = reader.saved_searches().unwrap();
    let unread = searches.iter().find(|s| s.name == "Unread local").unwrap();
    assert_eq!(unread.query, "is:unread from:ben@example.org");
    assert_eq!(unread.folders, Some(vec![folder("Inbox"), folder("2024")]));
    let big = searches.iter().find(|s| s.name == "Big mail").unwrap();
    assert_eq!(big.folders, None);

    // Contacts from the address book (WAL included).
    let carla = reader
        .contact_by_email("carla@example.net")
        .unwrap()
        .unwrap();
    assert_eq!(carla.display_name.as_deref(), Some("Carla Muster"));
    assert_eq!(carla.book, "Personal Address Book");
    let dora = reader
        .contact_by_email("dora@example.com")
        .unwrap()
        .unwrap();
    assert_eq!(dora.display_name.as_deref(), Some("Dora Vcard"));
    assert!(dora.vcard.unwrap().contains("BEGIN:VCARD"));
    drop(reader);

    // Running the import again changes nothing.
    let again = import(
        &profile,
        &env.ingestor,
        None,
        &ImportOptions::default(),
        &import::NoProgress,
    )
    .unwrap();
    assert_eq!(again.accounts_created, 0);
    assert_eq!(again.accounts.len(), 3);
    assert_eq!(again.identities, 0);
    assert_eq!(again.tags, 0);
    assert_eq!(again.rules, 0);
    assert_eq!(again.saved_searches, 0);
    assert_eq!(again.messages.imported, 0);
    assert_eq!(again.messages.duplicates, 4);
    let reader = env.store.reader().unwrap();
    assert_eq!(reader.accounts().unwrap().len(), 3);
    assert_eq!(reader.identities(None).unwrap().len(), 3);
    assert_eq!(reader.smtp_servers().unwrap().len(), 2);
    drop(reader);
    env.close();
}

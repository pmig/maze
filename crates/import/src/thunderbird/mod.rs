//! Thunderbird profile import (PLAN §4.14, CONCEPT §17).
//!
//! 1. Find the profile: [`ProfileLocator`] (`profiles.ini`, `[Install…] Default`).
//! 2. Load it: [`ThunderbirdProfile::load`] parses `prefs.js` into a
//!    [`ThunderbirdConfig`] and reads `virtualFolders.dat`, every server's
//!    `msgFilterRules.dat`, `xulstore.json`, and locates the address books.
//! 3. Show the dry run: [`ThunderbirdProfile::report`] builds an [`ImportReport`]
//!    (accounts, which need a new sign-in or a password, unsupported items, counts and
//!    sizes) without writing anything.
//! 4. Import: [`import`] creates SMTP servers, IMAP accounts (Gmail and Microsoft 365
//!    included), identities, signatures and tags, address books as contacts, filters
//!    as **disabled** rules, virtual folders as saved searches, and optionally builds
//!    the offline [`BodyReuse`] index from `ImapMail/**`, so the first sync takes
//!    bodies from disk. Running it again doesn't duplicate anything: accounts are
//!    recognized by their Thunderbird origin, rules, saved searches, tags and contacts
//!    by their keys.
//!
//! Maze is IMAP only (ADR-019): POP3 accounts, Local Folders and other account types
//! are skipped, each with a sentence saying why and what to do instead
//! ([`ImportOutcome::skipped`], [`ImportReport::unsupported`]), and so are their
//! filters.
//!
//! Nothing is written to the profile; it stays usable by Thunderbird (side-by-side
//! mode).

pub mod abook;
pub mod filters;
pub mod model;
pub mod prefs;
pub mod profiles;
pub mod virtual_folders;
pub mod xulstore;

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use mazemail_types::AccountId;
use serde::Serialize;
use serde_json::json;
use store::{
    AccountKind, IdentityInput, NewAccount, Queries, RuleInput, SavesSent, SignatureId,
    SignatureInput, SmtpAuth, SmtpSecurity, SmtpServerId, SmtpServerInput, Store,
};

use self::filters::FilterFile;
use self::model::{AuthMethod, ServerType, SocketType, TbServer, ThunderbirdConfig};
use self::prefs::Prefs;
use self::virtual_folders::VirtualFolder;
use self::xulstore::LayoutSuggestion;
use crate::progress::{Phase, Progress, Unit, report};
use crate::reuse::{BodyReuse, ScanStats};
use crate::{Error, Result};

pub use self::model::TbAccount;
pub use self::profiles::{ProfileInfo, ProfileLocator};

/// A loaded Thunderbird profile.
#[derive(Debug, Clone)]
pub struct ThunderbirdProfile {
    pub path: PathBuf,
    pub prefs: Prefs,
    pub config: ThunderbirdConfig,
    pub virtual_folders: Vec<VirtualFolder>,
    /// `msgFilterRules.dat` per server key.
    pub filters: BTreeMap<String, FilterFile>,
    /// Address book files and their names (read during report and import).
    pub address_books: Vec<(PathBuf, String)>,
    pub layout: Option<LayoutSuggestion>,
}

impl ThunderbirdProfile {
    /// Loads the profile at `path`. Fails if there is no `prefs.js`; every other file
    /// is optional and read tolerantly.
    pub fn load(path: &Path) -> Result<Self> {
        let prefs_path = path.join("prefs.js");
        let text = match fs::read(&prefs_path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NoProfile(path.to_owned()));
            }
            Err(err) => return Err(Error::io("reading", prefs_path)(err)),
        };
        let mut prefs = Prefs::parse(&text);
        // `user.js` overrides `prefs.js` at startup.
        if let Ok(user) = fs::read_to_string(path.join("user.js")) {
            prefs.merge(Prefs::parse(&user));
        }
        let config = ThunderbirdConfig::from_prefs(&prefs, path);

        let virtual_folders = read_optional(&path.join("virtualFolders.dat"))
            .map(|t| virtual_folders::parse(&t))
            .unwrap_or_default();
        let mut filter_files = BTreeMap::new();
        for (key, server) in &config.servers {
            if let Some(dir) = &server.directory
                && let Some(text) = read_optional(&dir.join("msgFilterRules.dat"))
            {
                filter_files.insert(key.clone(), filters::parse(&text));
            }
        }
        let mut layout =
            read_optional(&path.join("xulstore.json")).and_then(|t| xulstore::parse(&t));
        if config.pane_layout.is_some() || config.thread_pane_list_view.is_some() {
            let l = layout.get_or_insert_with(LayoutSuggestion::default);
            l.pane_layout = config.pane_layout;
            l.thread_pane_list_view = config.thread_pane_list_view;
        }
        Ok(Self {
            path: path.to_owned(),
            address_books: abook::find(path, &prefs),
            prefs,
            config,
            virtual_folders,
            filters: filter_files,
            layout,
        })
    }

    /// The dry-run report: what [`import`] would do. Reads address books (to count
    /// contacts) and walks mail directories (for sizes); writes nothing.
    pub fn report(&self) -> Result<ImportReport> {
        let mut report = ImportReport {
            profile: self.path.clone(),
            accounts: Vec::new(),
            smtp_servers: self
                .config
                .smtp_servers
                .iter()
                .filter(|s| s.kind == "smtp" && s.hostname.is_some())
                .count(),
            identities: 0,
            signatures: 0,
            tags: self.config.tags.len(),
            saved_searches: self.virtual_folders.len(),
            untranslated_search_terms: 0,
            filters: 0,
            filters_with_unsupported_actions: 0,
            address_books: Vec::new(),
            offline_store_bytes: 0,
            layout: self.layout.clone(),
            unsupported: Vec::new(),
        };
        for account in &self.config.accounts {
            let Some(server) = self.config.server_of(account) else {
                continue;
            };
            let plan = AccountPlan::new(account, server);
            match plan.kind {
                PlannedKind::Unsupported => report.unsupported.push(skipped_account(&plan, server)),
                PlannedKind::Imap | PlannedKind::Gmail | PlannedKind::M365 => {
                    report.offline_store_bytes += server
                        .directory
                        .as_deref()
                        .map(dir_size)
                        .unwrap_or_default();
                }
            }
            if plan.kind != PlannedKind::Unsupported {
                report.identities += account.identities.len();
                report.signatures += account
                    .identities
                    .iter()
                    .filter_map(|k| self.config.identities.get(k))
                    .filter(|i| {
                        i.signature.text.is_some()
                            || (i.signature.use_file && i.signature.file.is_some())
                    })
                    .count();
            }
            report.accounts.push(plan);
        }
        for folder in &self.virtual_folders {
            report.untranslated_search_terms += virtual_folders::to_query(&folder.terms).1.len();
        }
        let mut skipped_filters = 0;
        for (server, file) in &self.filters {
            if !self.imports_server(server) {
                skipped_filters += file.filters.len();
                continue;
            }
            report.filters += file.filters.len();
            report.filters_with_unsupported_actions += file
                .filters
                .iter()
                .filter(|f| !f.unsupported_actions().is_empty())
                .count();
        }
        if skipped_filters > 0 {
            report.unsupported.push(Unsupported {
                item: format!("{skipped_filters} message filters"),
                reason: "skipped: they belong to accounts Maze doesn't import".to_owned(),
            });
        }
        if report.filters > 0 {
            report.unsupported.push(Unsupported {
                item: format!("{} message filters", report.filters),
                reason: "imported disabled; review and enable them once Maze's rules engine is available"
                    .to_owned(),
            });
        }
        for (path, name) in &self.address_books {
            match abook::read(path, name) {
                Ok(book) => {
                    if !book.lists.is_empty() {
                        report.unsupported.push(Unsupported {
                            item: format!("{} mailing lists in {}", book.lists.len(), book.name),
                            reason: "Maze has no contact lists yet; their members are imported as contacts"
                                .to_owned(),
                        });
                    }
                    report.address_books.push(BookPlan {
                        name: book.name,
                        contacts: book.cards.iter().map(|c| c.emails.len()).sum(),
                        lists: book.lists.len(),
                    });
                }
                Err(err) => report.unsupported.push(Unsupported {
                    item: format!("address book {name}"),
                    reason: format!("can't be read: {err}"),
                }),
            }
        }
        for (dir, what) in [
            ("calendar-data", "calendars and tasks"),
            ("chat", "chat accounts"),
        ] {
            if self.path.join(dir).exists() {
                report.unsupported.push(Unsupported {
                    item: what.to_owned(),
                    reason: "out of scope for Maze".to_owned(),
                });
            }
        }
        Ok(report)
    }

    /// Whether the server's account is imported (IMAP, Gmail, Microsoft 365). Servers
    /// without an account in the profile count as imported, as they always did.
    fn imports_server(&self, server: &str) -> bool {
        self.config
            .servers
            .get(server)
            .is_none_or(|s| planned_kind(s) != PlannedKind::Unsupported)
    }
}

/// Why an account isn't imported, and what to do instead (ADR-019).
fn skipped_account(plan: &AccountPlan, server: &TbServer) -> Unsupported {
    let label = plan.label();
    match server.kind {
        ServerType::Pop3 => Unsupported {
            item: format!("POP3 account {label}"),
            reason: "skipped: Maze supports IMAP only — enable IMAP at your provider and add it"
                .to_owned(),
        },
        ServerType::Local => Unsupported {
            item: "Local Folders".to_owned(),
            reason: "skipped: Maze keeps all mail on your servers — move it into an IMAP folder \
                     in Thunderbird, or import its mbox files with \
                     `mazemail import <file> --account …`"
                .to_owned(),
        },
        _ => Unsupported {
            item: format!("{} account {label}", server.kind.label()),
            reason: "skipped: Maze supports IMAP accounts only".to_owned(),
        },
    }
}

fn read_optional(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Bytes of mail stores below `dir` (ignores Thunderbird's `.msf` summaries).
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| {
            let path = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => dir_size(&path),
                Ok(t) if t.is_file() && path.extension().is_none_or(|x| x != "msf") => {
                    e.metadata().map(|m| m.len()).unwrap_or(0)
                }
                _ => 0,
            }
        })
        .sum()
}

// ---------------------------------------------------------------------------------------
// Report

/// What Maze will make of a Thunderbird account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannedKind {
    Imap,
    Gmail,
    M365,
    /// POP3, Local Folders, news, feeds, Exchange (EWS) and other types: not imported
    /// (Maze is IMAP only, ADR-019).
    Unsupported,
}

/// How an imported account signs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPlan {
    /// OAuth2: tokens are bound to Mozilla's client id; sign in again.
    SignInAgain,
    /// Password: Maze asks once and keeps it in the keychain.
    PasswordPrompt,
    /// Nothing to sign in to (skipped accounts, servers without authentication).
    None,
    /// Kerberos, NTLM or client certificates: set up manually.
    Manual,
}

/// One account in the dry run.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct AccountPlan {
    pub key: String,
    pub name: String,
    pub kind: PlannedKind,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub username: Option<String>,
    pub auth: AuthPlan,
    pub identities: usize,
}

impl fmt::Debug for AccountPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountPlan")
            .field("key", &self.key)
            .field("kind", &self.kind)
            .field("auth", &self.auth)
            .field("identities", &self.identities)
            .finish_non_exhaustive()
    }
}

impl AccountPlan {
    fn new(account: &TbAccount, server: &TbServer) -> Self {
        let kind = planned_kind(server);
        let auth = match (kind, server.auth) {
            (PlannedKind::Unsupported, _) => AuthPlan::None,
            (_, AuthMethod::OAuth2) => AuthPlan::SignInAgain,
            (_, AuthMethod::Password | AuthMethod::Unknown(_)) => AuthPlan::PasswordPrompt,
            (_, AuthMethod::None) => AuthPlan::None,
            (_, AuthMethod::Gssapi | AuthMethod::Ntlm | AuthMethod::External) => AuthPlan::Manual,
        };
        Self {
            key: account.key.clone(),
            name: server
                .name
                .clone()
                .or_else(|| server.username.clone())
                .or_else(|| server.hostname.clone())
                .unwrap_or_else(|| account.key.clone()),
            kind,
            host: server.hostname.clone(),
            port: server.port,
            username: server.username.clone(),
            auth,
            identities: account.identities.len(),
        }
    }

    fn label(&self) -> String {
        self.name.clone()
    }
}

fn planned_kind(server: &TbServer) -> PlannedKind {
    match server.kind {
        ServerType::Imap => {
            let host = server
                .hostname
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if host.ends_with("gmail.com") || host.ends_with("googlemail.com") {
                PlannedKind::Gmail
            } else if host == "outlook.office365.com" || host.ends_with(".office365.com") {
                PlannedKind::M365
            } else {
                PlannedKind::Imap
            }
        }
        _ => PlannedKind::Unsupported,
    }
}

/// An address book in the dry run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BookPlan {
    pub name: String,
    /// Contacts (one per address).
    pub contacts: usize,
    pub lists: usize,
}

/// Something that won't be imported (or only partly), with the reason.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Unsupported {
    pub item: String,
    pub reason: String,
}

impl fmt::Display for Unsupported {
    /// The sentence shown in the import result ("POP3 account x@y skipped: …").
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.item, self.reason)
    }
}

impl fmt::Debug for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Unsupported")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

/// The dry-run report shown before importing (PLAN §4.14 item 7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    pub profile: PathBuf,
    pub accounts: Vec<AccountPlan>,
    pub smtp_servers: usize,
    pub identities: usize,
    pub signatures: usize,
    pub tags: usize,
    pub saved_searches: usize,
    /// Search terms of saved searches without a Maze equivalent (left out).
    pub untranslated_search_terms: usize,
    pub filters: usize,
    pub filters_with_unsupported_actions: usize,
    pub address_books: Vec<BookPlan>,
    /// Size of the offline IMAP stores the body index will scan.
    pub offline_store_bytes: u64,
    pub layout: Option<LayoutSuggestion>,
    pub unsupported: Vec<Unsupported>,
}

impl ImportReport {
    /// Accounts that need a new sign-in (OAuth2).
    pub fn sign_in_again(&self) -> impl Iterator<Item = &AccountPlan> {
        self.accounts
            .iter()
            .filter(|a| a.auth == AuthPlan::SignInAgain)
    }

    /// Accounts whose password Maze will ask for.
    pub fn password_prompts(&self) -> impl Iterator<Item = &AccountPlan> {
        self.accounts
            .iter()
            .filter(|a| a.auth == AuthPlan::PasswordPrompt)
    }
}

// ---------------------------------------------------------------------------------------
// Import

/// What to import; everything by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportOptions {
    pub accounts: bool,
    pub address_books: bool,
    pub filters: bool,
    pub saved_searches: bool,
    pub tags: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            accounts: true,
            address_books: true,
            filters: true,
            saved_searches: true,
            tags: true,
        }
    }
}

/// What [`import`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportOutcome {
    /// Thunderbird account key → Maze account.
    pub accounts: BTreeMap<String, AccountId>,
    pub accounts_created: usize,
    pub smtp_servers: usize,
    pub identities: usize,
    pub signatures: usize,
    /// Tags created or renamed/recolored to Thunderbird's.
    pub tags: usize,
    pub contacts: usize,
    pub rules: usize,
    pub saved_searches: usize,
    /// Accounts and filters not imported, one sentence each ("POP3 account x@y
    /// skipped: Maze supports IMAP only — enable IMAP at your provider and add it").
    pub skipped: Vec<String>,
    pub body_index: Option<ScanStats>,
    pub layout: Option<LayoutSuggestion>,
    /// Non-fatal problems (unreadable files and the like), for the activity log.
    pub warnings: Vec<String>,
}

/// Marker stored in an imported account's config, to recognize it on a re-run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
struct Origin {
    source: String,
    profile: String,
    account: String,
}

/// Imports a loaded profile (see the module docs). If `reuse` is given, the offline
/// stores of the IMAP accounts are scanned into it.
pub fn import(
    profile: &ThunderbirdProfile,
    store: &Store,
    reuse: Option<&BodyReuse>,
    options: &ImportOptions,
    progress: &dyn Progress,
) -> Result<ImportOutcome> {
    let config = &profile.config;
    let mut out = ImportOutcome {
        layout: profile.layout.clone(),
        ..ImportOutcome::default()
    };
    report(
        progress,
        Phase::Accounts,
        0,
        config.accounts.len() as u64,
        Unit::Items,
    );

    // SMTP servers (only for new accounts; reused accounts keep theirs).
    let mut smtp: HashMap<String, SmtpServerId> = HashMap::new();

    if options.accounts {
        let existing = store.reader()?.accounts()?;
        let profile_key = profile.path.to_string_lossy().into_owned();
        let mut new_accounts: Vec<(&TbAccount, &TbServer, AccountId)> = Vec::new();
        for (ordinal, account) in config.accounts.iter().enumerate() {
            let Some(server) = config.server_of(account) else {
                continue;
            };
            let plan = AccountPlan::new(account, server);
            let kind = match plan.kind {
                PlannedKind::Imap => AccountKind::Imap,
                PlannedKind::Gmail => AccountKind::Gmail,
                PlannedKind::M365 => AccountKind::M365,
                PlannedKind::Unsupported => {
                    out.skipped.push(skipped_account(&plan, server).to_string());
                    continue;
                }
            };
            let origin = Origin {
                source: "thunderbird".to_owned(),
                profile: profile_key.clone(),
                account: account.key.clone(),
            };
            if let Some(found) = existing.iter().find(|a| {
                serde_json::from_str::<serde_json::Value>(&a.config_json)
                    .ok()
                    .and_then(|v| serde_json::from_value::<Origin>(v.get("import")?.clone()).ok())
                    .is_some_and(|o| o == origin)
            }) {
                out.accounts.insert(account.key.clone(), found.id);
                continue;
            }
            let config_json = account_config(config, account, server, &origin);
            let new = NewAccount {
                kind,
                name: plan.name.clone(),
                config_json,
                ordinal: existing.len() as i64 + ordinal as i64,
            };
            let id = store.write_blocking(move |w| w.create_account(&new))?;
            out.accounts.insert(account.key.clone(), id);
            out.accounts_created += 1;
            new_accounts.push((account, server, id));
        }

        // SMTP servers used by identities of new accounts.
        let used: Vec<String> = new_accounts
            .iter()
            .flat_map(|(a, _, _)| a.identities.iter())
            .filter_map(|k| config.identities.get(k)?.smtp_server.clone())
            .chain(
                config
                    .default_smtp_server
                    .clone()
                    .filter(|_| !new_accounts.is_empty()),
            )
            .collect();
        let existing_smtp = store.reader()?.smtp_servers()?;
        for server in &config.smtp_servers {
            if server.kind != "smtp" || !used.contains(&server.key) {
                continue;
            }
            let Some(host) = server.hostname.clone() else {
                continue;
            };
            let input = SmtpServerInput {
                name: server.description.clone(),
                host,
                port: server.port,
                security: match server.socket {
                    SocketType::Tls => SmtpSecurity::Tls,
                    SocketType::StartTls => SmtpSecurity::StartTls,
                    SocketType::Plain | SocketType::Unknown(_) => SmtpSecurity::None,
                },
                auth: match server.auth {
                    AuthMethod::OAuth2 => SmtpAuth::OAuth2,
                    AuthMethod::None => SmtpAuth::None,
                    _ => SmtpAuth::Plain,
                },
                username: server.username.clone(),
                saves_sent: SavesSent::Auto,
            };
            // The same server imported before (or set up by hand) is reused.
            if let Some(found) = existing_smtp.iter().find(|s| {
                s.host.eq_ignore_ascii_case(&input.host)
                    && s.port == input.port
                    && s.username == input.username
            }) {
                smtp.insert(server.key.clone(), found.id);
                continue;
            }
            let id = store.write_blocking(move |w| w.create_smtp_server(&input))?;
            smtp.insert(server.key.clone(), id);
            out.smtp_servers += 1;
        }

        // Identities and signatures. Their folders (Sent, Drafts, Archive) exist only
        // after the first sync; the connector reads them from the account config.
        let default_account = config.default_account.clone();
        let mut identity_ordinal = 0i64;
        for (account, _, id) in &new_accounts {
            for (index, key) in account.identities.iter().enumerate() {
                let Some(identity) = config.identities.get(key) else {
                    continue;
                };
                let Some(email) = identity.email.clone() else {
                    continue;
                };
                let signature_text = match (&identity.signature.file, identity.signature.use_file) {
                    (Some(file), true) => match fs::read(file) {
                        Ok(bytes) => Some((
                            String::from_utf8_lossy(&bytes).into_owned(),
                            file.extension().is_some_and(|e| e == "html" || e == "htm"),
                        )),
                        Err(err) => {
                            out.warnings.push(format!(
                                "signature file {} can't be read: {err}",
                                file.display()
                            ));
                            None
                        }
                    },
                    _ => identity
                        .signature
                        .text
                        .clone()
                        .map(|t| (t, identity.signature.is_html)),
                };
                let signature: Option<SignatureId> = match signature_text {
                    Some((text, is_html)) => {
                        let input = SignatureInput {
                            name: identity
                                .full_name
                                .clone()
                                .map_or_else(|| email.clone(), |n| format!("{n} <{email}>")),
                            body_html: is_html.then(|| text.clone()),
                            body_text: (!is_html).then_some(text),
                            ordinal: identity_ordinal,
                        };
                        out.signatures += 1;
                        Some(store.write_blocking(move |w| w.create_signature(&input))?)
                    }
                    None => None,
                };
                let input = IdentityInput {
                    label: None,
                    display_name: identity.full_name.clone(),
                    email,
                    reply_to: identity.reply_to.clone(),
                    signature,
                    smtp: identity
                        .smtp_server
                        .as_ref()
                        .or(config.default_smtp_server.as_ref())
                        .and_then(|k| smtp.get(k).copied()),
                    sent_folder: None,
                    drafts_folder: None,
                    archive_folder: None,
                    archive_granularity: if identity.archive_enabled {
                        identity.archive_granularity
                    } else {
                        0
                    },
                    color: None,
                    match_patterns_json: (identity.catch_all
                        && !identity.catch_all_hint.is_empty())
                    .then(|| json!(identity.catch_all_hint).to_string()),
                    is_default: index == 0
                        && default_account.as_deref() == Some(account.key.as_str()),
                    ordinal: identity_ordinal,
                };
                identity_ordinal += 1;
                let account_id = *id;
                store.write_blocking(move |w| w.create_identity(account_id, &input))?;
                out.identities += 1;
            }
        }
        report(
            progress,
            Phase::Accounts,
            config.accounts.len() as u64,
            config.accounts.len() as u64,
            Unit::Items,
        );
    }

    if options.tags {
        let tags = config.tags.clone();
        out.tags = store.write_blocking(move |w| {
            let mut created = 0;
            for (i, tag) in tags.iter().enumerate() {
                match w.tag_by_keyword(&tag.keyword)? {
                    // Thunderbird's name and color win (the store seeds the defaults).
                    Some(existing) => {
                        if existing.name != tag.name || existing.color != tag.color {
                            w.update_tag(
                                existing.id,
                                &tag.name,
                                tag.color.as_deref(),
                                existing.ordinal,
                            )?;
                            created += 1;
                        }
                    }
                    None => {
                        w.create_tag(&tag.keyword, &tag.name, tag.color.as_deref(), i as i64 + 1)?;
                        created += 1;
                    }
                }
            }
            Ok(created)
        })?;
    }

    if options.address_books {
        let total = profile.address_books.len() as u64;
        for (i, (path, name)) in profile.address_books.iter().enumerate() {
            match abook::read(path, name) {
                Ok(book) => {
                    let book_name = book.name.clone();
                    let cards = book.cards;
                    out.contacts += store.write_blocking(move |w| {
                        let mut n = 0;
                        for card in &cards {
                            for email in &card.emails {
                                w.upsert_contact(
                                    &book_name,
                                    email,
                                    card.display_name.as_deref(),
                                    card.vcard.as_deref(),
                                )?;
                                n += 1;
                            }
                        }
                        Ok(n)
                    })?;
                }
                Err(err) => out
                    .warnings
                    .push(format!("address book {name} can't be read: {err}")),
            }
            report(progress, Phase::Contacts, i as u64 + 1, total, Unit::Items);
        }
    }

    if options.filters {
        let existing = store.reader()?.rules(None)?;
        let mut ordinal = existing.len() as i64;
        let mut inputs = Vec::new();
        let mut skipped = 0;
        for (server_key, file) in &profile.filters {
            if !profile.imports_server(server_key) {
                skipped += file.filters.len();
                continue;
            }
            let account = config
                .account_for_server(server_key)
                .and_then(|a| out.accounts.get(&a.key).copied());
            for filter in &file.filters {
                if existing
                    .iter()
                    .any(|r| r.input.account == account && r.input.name == filter.name)
                {
                    continue;
                }
                inputs.push(rule_input(filter, account, server_key, ordinal));
                ordinal += 1;
            }
        }
        if skipped > 0 {
            out.skipped.push(format!(
                "{skipped} message filters skipped: they belong to accounts Maze doesn't import"
            ));
        }
        out.rules = inputs.len();
        store.write_blocking(move |w| {
            for input in &inputs {
                w.create_rule(input)?;
            }
            Ok(())
        })?;
    }

    if options.saved_searches {
        let existing = store.reader()?.saved_searches()?;
        let mut ordinal = existing.len() as i64;
        let mut inputs = Vec::new();
        for folder in &profile.virtual_folders {
            if existing.iter().any(|s| s.name == folder.name) {
                continue;
            }
            let (query, _) = virtual_folders::to_query(&folder.terms);
            // IMAP folders exist only after the first sync, so a saved search covers
            // every folder until it is edited.
            inputs.push((folder.name.clone(), query, None::<Vec<_>>, ordinal));
            ordinal += 1;
        }
        out.saved_searches = inputs.len();
        store.write_blocking(move |w| {
            for (name, query, folders, ordinal) in &inputs {
                w.create_saved_search(name, query, folders.as_deref(), *ordinal)?;
            }
            Ok(())
        })?;
        report(progress, Phase::Rules, 1, 1, Unit::Items);
    }

    if let Some(reuse) = reuse {
        let roots: Vec<PathBuf> = config
            .accounts
            .iter()
            .filter_map(|a| config.server_of(a))
            .filter(|s| s.kind == ServerType::Imap)
            .filter_map(|s| s.directory.clone())
            .filter(|d| d.is_dir())
            .collect();
        out.body_index = Some(reuse.scan(&roots, progress)?);
    }
    Ok(out)
}

/// The connector config of a new account (owned by the connectors; the importer
/// writes what Thunderbird knew plus the import origin).
fn account_config(
    config: &ThunderbirdConfig,
    account: &TbAccount,
    server: &TbServer,
    origin: &Origin,
) -> String {
    let folders: Vec<serde_json::Value> = account
        .identities
        .iter()
        .filter_map(|k| config.identities.get(k))
        .map(|i| {
            json!({
                "email": i.email,
                "sent": i.fcc_folder,
                "drafts": i.draft_folder,
                "archive": i.archive_folder,
                "templates": i.templates_folder,
            })
        })
        .collect();
    json!({
        "host": server.hostname,
        "port": server.port,
        "security": match server.socket {
            SocketType::Tls => "tls",
            SocketType::StartTls => "starttls",
            SocketType::Plain | SocketType::Unknown(_) => "none",
        },
        "username": server.username,
        "auth": match server.auth {
            AuthMethod::OAuth2 => "oauth2",
            AuthMethod::None => "none",
            AuthMethod::Gssapi => "gssapi",
            AuthMethod::Ntlm => "ntlm",
            AuthMethod::External => "external",
            AuthMethod::Password | AuthMethod::Unknown(_) => "password",
        },
        "protocol": "imap",
        "identity_folders": folders,
        "import": origin,
    })
    .to_string()
}

/// A disabled rule from a Thunderbird filter (PLAN §4.13).
fn rule_input(
    filter: &filters::Filter,
    account: Option<AccountId>,
    server_key: &str,
    ordinal: i64,
) -> RuleInput {
    let unsupported: Vec<String> = filter
        .unsupported_actions()
        .iter()
        .map(|a| a.action.clone())
        .collect();
    let mut note =
        String::from("Imported from Thunderbird and disabled: review it before enabling.");
    if !filter.enabled {
        note.push_str(" It was disabled in Thunderbird.");
    }
    if !unsupported.is_empty() {
        note.push_str(&format!(
            " Unsupported actions: {}.",
            unsupported.join(", ")
        ));
    }
    let condition = json!({
        "source": "thunderbird",
        "match": if filter.condition.match_all { "all_messages" } else { "terms" },
        "terms": filter.condition.terms,
        "raw": filter.condition.raw,
        "thunderbird": {
            "server": server_key,
            "type": filter.filter_type,
            "triggers": filter.triggers(),
            "enabled": filter.enabled,
            "description": filter.description,
        },
        "import_note": note,
    });
    let actions: Vec<serde_json::Value> = filter
        .actions
        .iter()
        .map(|a| {
            json!({
                "action": a.kind(),
                "value": a.value,
                "custom_id": a.custom_id,
                "supported": a.is_supported(),
                "thunderbird": a.action,
            })
        })
        .collect();
    RuleInput {
        account,
        name: filter.name.clone(),
        ordinal,
        enabled: false,
        trigger: filter.primary_trigger(),
        condition_json: condition.to_string(),
        actions_json: serde_json::Value::Array(actions).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(kind: ServerType, hostname: &str) -> TbServer {
        TbServer {
            key: "server9".to_owned(),
            kind,
            hostname: Some(hostname.to_owned()),
            port: None,
            socket: SocketType::Plain,
            auth: AuthMethod::Password,
            username: Some("anna@example.com".to_owned()),
            name: None,
            directory: None,
            store_format: crate::tree::StoreFormat::Mbox,
            deferred_to_account: None,
        }
    }

    #[test]
    fn only_imap_servers_are_imported() {
        assert_eq!(
            planned_kind(&server(ServerType::Imap, "imap.gmail.com")),
            PlannedKind::Gmail
        );
        assert_eq!(
            planned_kind(&server(ServerType::Imap, "outlook.office365.com")),
            PlannedKind::M365
        );
        assert_eq!(
            planned_kind(&server(ServerType::Imap, "imap.example.com")),
            PlannedKind::Imap
        );
        for kind in [ServerType::Pop3, ServerType::Local, ServerType::Nntp] {
            assert_eq!(
                planned_kind(&server(kind, "x.example.com")),
                PlannedKind::Unsupported
            );
        }
    }

    #[test]
    fn skipped_accounts_say_why_and_what_to_do() {
        let pop = server(ServerType::Pop3, "pop.example.com");
        let account = TbAccount {
            key: "account9".to_owned(),
            server: Some("server9".to_owned()),
            identities: Vec::new(),
        };
        let plan = AccountPlan::new(&account, &pop);
        assert_eq!(plan.auth, AuthPlan::None);
        assert_eq!(
            skipped_account(&plan, &pop).to_string(),
            "POP3 account anna@example.com skipped: Maze supports IMAP only — enable IMAP at \
             your provider and add it"
        );
        let local = server(ServerType::Local, "Local Folders");
        let plan = AccountPlan::new(&account, &local);
        assert!(
            skipped_account(&plan, &local)
                .to_string()
                .starts_with("Local Folders skipped: ")
        );
    }
}

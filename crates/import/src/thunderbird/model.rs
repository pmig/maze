//! The typed model of a Thunderbird configuration, read from [`Prefs`].
//!
//! Pref names and value meanings follow `mailnews/mailnews.js`,
//! `MailNewsTypes2.idl` (socket types, auth methods) and `nsMsgIncomingServer.cpp`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::prefs::Prefs;
use crate::tree::StoreFormat;

/// Incoming server type (`mail.server.*.type`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerType {
    Imap,
    Pop3,
    /// `none`: Local Folders.
    Local,
    Nntp,
    Rss,
    /// Exchange Web Services and others Maze doesn't connect to.
    Other(String),
}

impl ServerType {
    /// A human-readable name of the account type.
    pub fn label(&self) -> &str {
        match self {
            Self::Imap => "IMAP",
            Self::Pop3 => "POP3",
            Self::Local => "Local Folders",
            Self::Nntp => "News (NNTP)",
            Self::Rss => "Feeds (RSS)",
            Self::Other(kind) => kind,
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "imap" => Self::Imap,
            "pop3" => Self::Pop3,
            "none" => Self::Local,
            "nntp" => Self::Nntp,
            "rss" => Self::Rss,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// Transport security (`socketType`, `try_ssl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SocketType {
    /// 0.
    Plain,
    /// 2 (1, "try STARTTLS", is obsolete and treated the same).
    StartTls,
    /// 3.
    Tls,
    Unknown(i64),
}

impl SocketType {
    fn from_pref(value: Option<i64>) -> Self {
        match value {
            None | Some(0) => Self::Plain,
            Some(1 | 2) => Self::StartTls,
            Some(3) => Self::Tls,
            Some(other) => Self::Unknown(other),
        }
    }
}

/// Authentication method (`authMethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// 1.
    None,
    /// 3 (cleartext) or 4 (encrypted), and the old values 2 and 8/9.
    Password,
    /// 10: OAuth2. Tokens are bound to Mozilla's client id, so the account signs in
    /// again in Maze.
    OAuth2,
    /// 5.
    Gssapi,
    /// 6.
    Ntlm,
    /// 7: TLS client certificate.
    External,
    Unknown(i64),
}

impl AuthMethod {
    fn from_pref(value: Option<i64>) -> Self {
        match value {
            Some(1) => Self::None,
            None | Some(2..=4 | 8 | 9) => Self::Password,
            Some(5) => Self::Gssapi,
            Some(6) => Self::Ntlm,
            Some(7) => Self::External,
            Some(10) => Self::OAuth2,
            Some(other) => Self::Unknown(other),
        }
    }
}

/// An incoming server (`mail.server.<key>.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TbServer {
    pub key: String,
    pub kind: ServerType,
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub socket: SocketType,
    pub auth: AuthMethod,
    pub username: Option<String>,
    /// The account name shown in the folder pane.
    pub name: Option<String>,
    /// The mail directory (`directory-rel` resolved against the profile).
    pub directory: Option<PathBuf>,
    pub store_format: StoreFormat,
    /// POP3 accounts that deliver into Local Folders (global inbox).
    pub deferred_to_account: Option<String>,
}

/// A signature of an identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TbSignature {
    /// `htmlSigText`.
    pub text: Option<String>,
    /// `htmlSigFormat`: the text is HTML.
    pub is_html: bool,
    /// `attach_signature` + `sig_file(-rel)`: the signature comes from this file.
    pub file: Option<PathBuf>,
    pub use_file: bool,
}

/// A sending identity (`mail.identity.<key>.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TbIdentity {
    pub key: String,
    pub full_name: Option<String>,
    pub email: Option<String>,
    pub reply_to: Option<String>,
    pub organization: Option<String>,
    pub signature: TbSignature,
    /// "Reply from this identity when delivery headers match".
    pub catch_all: bool,
    /// Patterns for `catch_all`, comma separated in Thunderbird.
    pub catch_all_hint: Vec<String>,
    pub archive_enabled: bool,
    /// 0 single folder, 1 yearly, 2 monthly.
    pub archive_granularity: i64,
    pub archive_keep_folder_structure: bool,
    /// Folder URIs.
    pub archive_folder: Option<String>,
    pub fcc_folder: Option<String>,
    pub draft_folder: Option<String>,
    pub templates_folder: Option<String>,
    /// The SMTP server key.
    pub smtp_server: Option<String>,
}

/// An account (`mail.account.<key>.*`): one server and its identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TbAccount {
    pub key: String,
    pub server: Option<String>,
    pub identities: Vec<String>,
}

/// An outgoing server (`mail.smtpserver.<key>.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TbSmtpServer {
    pub key: String,
    /// `smtp`, or `ews` for Exchange.
    pub kind: String,
    pub hostname: Option<String>,
    pub port: Option<u16>,
    pub socket: SocketType,
    pub auth: AuthMethod,
    pub username: Option<String>,
    pub description: Option<String>,
}

/// A tag (`mailnews.tags.<keyword>.*`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TbTag {
    /// The IMAP keyword, lowercase.
    pub keyword: String,
    pub name: String,
    pub color: Option<String>,
    pub ordinal: Option<String>,
}

/// Thunderbird's five default tags (English names; the real defaults are localized).
const DEFAULT_TAGS: [(&str, &str, &str); 5] = [
    ("$label1", "Important", "#FF0000"),
    ("$label2", "Work", "#FF9900"),
    ("$label3", "Personal", "#009900"),
    ("$label4", "To Do", "#3333FF"),
    ("$label5", "Later", "#993399"),
];

/// Everything Maze reads from `prefs.js`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ThunderbirdConfig {
    /// In Thunderbird's order (`mail.accountmanager.accounts`).
    pub accounts: Vec<TbAccount>,
    pub default_account: Option<String>,
    /// The server key of Local Folders.
    pub local_folders_server: Option<String>,
    pub servers: BTreeMap<String, TbServer>,
    pub identities: BTreeMap<String, TbIdentity>,
    /// In Thunderbird's order (`mail.smtpservers`).
    pub smtp_servers: Vec<TbSmtpServer>,
    pub default_smtp_server: Option<String>,
    pub tags: Vec<TbTag>,
    /// `mail.serverDefaultStoreContractID`.
    pub default_store: StoreFormat,
    /// `mail.threadpane.listview`: 0 cards, 1 table.
    pub thread_pane_list_view: Option<i64>,
    /// `mail.pane_config.dynamic`: 0 classic, 1 wide, 2 vertical.
    pub pane_layout: Option<i64>,
}

fn split_list(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn store_format(contract: Option<&str>) -> Option<StoreFormat> {
    contract.map(|c| {
        if c.contains("maildirstore") {
            StoreFormat::Maildir
        } else {
            StoreFormat::Mbox
        }
    })
}

/// Resolves a `*-rel` pref (`[ProfD]ImapMail/host`) or its absolute twin.
fn resolve_path(prefs: &Prefs, base: &str, profile: &Path) -> Option<PathBuf> {
    if let Some(rel) = prefs.string(&format!("{base}-rel")) {
        let rel = rel.strip_prefix("[ProfD]").unwrap_or(rel);
        return Some(profile.join(rel));
    }
    prefs
        .string(base)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

fn port(prefs: &Prefs, name: &str) -> Option<u16> {
    prefs
        .int(name)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0)
}

fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl ThunderbirdConfig {
    /// Builds the model from parsed prefs of the profile at `profile`.
    pub fn from_prefs(prefs: &Prefs, profile: &Path) -> Self {
        let default_store = store_format(prefs.string("mail.serverDefaultStoreContractID"))
            .unwrap_or(StoreFormat::Mbox);

        let mut servers = BTreeMap::new();
        for key in prefs.branch_keys("mail.server.") {
            if key == "default" {
                continue;
            }
            let p = |name: &str| format!("mail.server.{key}.{name}");
            let kind = ServerType::parse(prefs.string(&p("type")).unwrap_or("imap"));
            let hostname = nonempty(prefs.string(&p("hostname")));
            let directory = resolve_path(prefs, &p("directory"), profile).or_else(|| {
                let host = hostname.clone().unwrap_or_default();
                match kind {
                    ServerType::Imap => Some(profile.join("ImapMail").join(host)),
                    ServerType::Local => Some(profile.join("Mail").join("Local Folders")),
                    ServerType::Pop3 => Some(profile.join("Mail").join(host)),
                    _ => None,
                }
            });
            servers.insert(
                key.clone(),
                TbServer {
                    key: key.clone(),
                    hostname,
                    port: port(prefs, &p("port")),
                    socket: SocketType::from_pref(prefs.int(&p("socketType"))),
                    auth: AuthMethod::from_pref(prefs.int(&p("authMethod"))),
                    username: nonempty(prefs.string(&p("userName"))),
                    name: nonempty(prefs.string(&p("name"))),
                    directory,
                    store_format: store_format(prefs.string(&p("storeContractID")))
                        .unwrap_or(default_store),
                    deferred_to_account: nonempty(prefs.string(&p("deferred_to_account"))),
                    kind,
                },
            );
        }

        let mut identities = BTreeMap::new();
        for key in prefs.branch_keys("mail.identity.") {
            if key == "default" {
                continue;
            }
            let p = |name: &str| format!("mail.identity.{key}.{name}");
            identities.insert(
                key.clone(),
                TbIdentity {
                    key: key.clone(),
                    full_name: nonempty(prefs.string(&p("fullName"))),
                    email: nonempty(prefs.string(&p("useremail"))),
                    reply_to: nonempty(prefs.string(&p("reply_to"))),
                    organization: nonempty(prefs.string(&p("organization"))),
                    signature: TbSignature {
                        text: nonempty(prefs.string(&p("htmlSigText"))),
                        is_html: prefs.bool(&p("htmlSigFormat")).unwrap_or(false),
                        file: resolve_path(prefs, &p("sig_file"), profile),
                        use_file: prefs.bool(&p("attach_signature")).unwrap_or(false),
                    },
                    catch_all: prefs.bool(&p("catchAll")).unwrap_or(false),
                    catch_all_hint: split_list(prefs.string(&p("catchAllHint"))),
                    archive_enabled: prefs.bool(&p("archive_enabled")).unwrap_or(true),
                    archive_granularity: prefs.int(&p("archive_granularity")).unwrap_or(1),
                    archive_keep_folder_structure: prefs
                        .bool(&p("archive_keep_folder_structure"))
                        .unwrap_or(false),
                    archive_folder: nonempty(prefs.string(&p("archive_folder"))),
                    fcc_folder: nonempty(prefs.string(&p("fcc_folder"))),
                    draft_folder: nonempty(prefs.string(&p("draft_folder"))),
                    templates_folder: nonempty(prefs.string(&p("stationery_folder"))),
                    smtp_server: nonempty(prefs.string(&p("smtpServer"))),
                },
            );
        }

        let mut account_keys = split_list(prefs.string("mail.accountmanager.accounts"));
        for key in prefs.branch_keys("mail.account.") {
            if key != "default" && !account_keys.contains(&key) {
                account_keys.push(key);
            }
        }
        let accounts = account_keys
            .into_iter()
            .filter(|key| {
                prefs
                    .string(&format!("mail.account.{key}.server"))
                    .is_some()
                    || prefs
                        .string(&format!("mail.account.{key}.identities"))
                        .is_some()
            })
            .map(|key| TbAccount {
                server: nonempty(prefs.string(&format!("mail.account.{key}.server"))),
                identities: split_list(prefs.string(&format!("mail.account.{key}.identities"))),
                key,
            })
            .collect();

        let mut smtp_keys = split_list(prefs.string("mail.smtpservers"));
        for key in prefs.branch_keys("mail.smtpserver.") {
            if key != "default" && !smtp_keys.contains(&key) {
                smtp_keys.push(key);
            }
        }
        let smtp_servers = smtp_keys
            .into_iter()
            .map(|key| {
                let p = |name: &str| format!("mail.smtpserver.{key}.{name}");
                TbSmtpServer {
                    kind: prefs.string(&p("type")).unwrap_or("smtp").to_owned(),
                    hostname: nonempty(prefs.string(&p("hostname"))),
                    port: port(prefs, &p("port")),
                    socket: SocketType::from_pref(prefs.int(&p("try_ssl"))),
                    auth: AuthMethod::from_pref(prefs.int(&p("authMethod"))),
                    username: nonempty(prefs.string(&p("username"))),
                    description: nonempty(prefs.string(&p("description"))),
                    key,
                }
            })
            .collect();

        let mut tags: Vec<TbTag> = DEFAULT_TAGS
            .iter()
            .map(|(keyword, name, color)| TbTag {
                keyword: (*keyword).to_owned(),
                name: (*name).to_owned(),
                color: Some((*color).to_owned()),
                ordinal: None,
            })
            .collect();
        for keyword in prefs.branch_keys("mailnews.tags.") {
            let p = |name: &str| format!("mailnews.tags.{keyword}.{name}");
            let Some(name) = prefs.string(&p("tag")) else {
                continue;
            };
            let tag = TbTag {
                keyword: keyword.to_lowercase(),
                name: name.to_owned(),
                color: nonempty(prefs.string(&p("color"))),
                ordinal: nonempty(prefs.string(&p("ordinal"))),
            };
            match tags.iter_mut().find(|t| t.keyword == tag.keyword) {
                Some(existing) => *existing = tag,
                None => tags.push(tag),
            }
        }

        Self {
            accounts,
            default_account: nonempty(prefs.string("mail.accountmanager.defaultaccount")),
            local_folders_server: nonempty(prefs.string("mail.accountmanager.localfoldersserver")),
            servers,
            identities,
            smtp_servers,
            default_smtp_server: nonempty(prefs.string("mail.smtp.defaultserver")),
            tags,
            default_store,
            thread_pane_list_view: prefs.int("mail.threadpane.listview"),
            pane_layout: prefs.int("mail.pane_config.dynamic"),
        }
    }

    /// The server of an account.
    pub fn server_of(&self, account: &TbAccount) -> Option<&TbServer> {
        account
            .server
            .as_ref()
            .and_then(|key| self.servers.get(key))
    }

    /// The account whose server has this key.
    pub fn account_for_server(&self, server: &str) -> Option<&TbAccount> {
        self.accounts
            .iter()
            .find(|a| a.server.as_deref() == Some(server))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_enums() {
        assert_eq!(SocketType::from_pref(Some(3)), SocketType::Tls);
        assert_eq!(SocketType::from_pref(Some(2)), SocketType::StartTls);
        assert_eq!(SocketType::from_pref(None), SocketType::Plain);
        assert_eq!(AuthMethod::from_pref(Some(10)), AuthMethod::OAuth2);
        assert_eq!(AuthMethod::from_pref(Some(3)), AuthMethod::Password);
        assert_eq!(AuthMethod::from_pref(Some(42)), AuthMethod::Unknown(42));
        assert_eq!(ServerType::parse("none"), ServerType::Local);
        assert_eq!(
            store_format(Some("@mozilla.org/msgstore/maildirstore;1")),
            Some(StoreFormat::Maildir)
        );
    }

    #[test]
    fn minimal_profile() {
        let prefs = Prefs::parse(
            r##"user_pref("mail.accountmanager.accounts", "account2,account1");
user_pref("mail.account.account1.server", "server1");
user_pref("mail.account.account2.server", "server2");
user_pref("mail.account.account2.identities", "id1");
user_pref("mail.server.server1.type", "none");
user_pref("mail.server.server2.hostname", "imap.example.com");
user_pref("mail.server.server2.directory-rel", "[ProfD]ImapMail/imap.example.com");
user_pref("mail.identity.id1.useremail", "me@example.com");
user_pref("mailnews.tags.$label1.tag", "Wichtig");
user_pref("mailnews.tags.projekt.tag", "Projekt");
user_pref("mailnews.tags.projekt.color", "#123456");"##,
        );
        let config = ThunderbirdConfig::from_prefs(&prefs, Path::new("/p"));
        assert_eq!(config.accounts[0].key, "account2");
        assert_eq!(config.servers["server2"].kind, ServerType::Imap);
        assert_eq!(
            config.servers["server2"].directory.as_deref(),
            Some(Path::new("/p/ImapMail/imap.example.com"))
        );
        assert_eq!(
            config.servers["server1"].directory.as_deref(),
            Some(Path::new("/p/Mail/Local Folders"))
        );
        assert_eq!(config.tags.len(), 6);
        assert_eq!(config.tags[0].name, "Wichtig");
        assert_eq!(config.identities["id1"].archive_granularity, 1);
        assert!(config.identities["id1"].archive_enabled);
    }
}

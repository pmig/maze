//! The typed settings (CONCEPT §16, PLAN §4.9, §4.15, §4.18).
//!
//! Every struct has `#[serde(default)]` and a [`Default`] impl that matches
//! `assets/settings/default.jsonc` (a test keeps the two in sync). The defaults file is
//! what people read; the `Default` impls are the fallback when a value can't be used.
//! Doc comments become descriptions in the generated JSON Schema.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mazemail_types::MicrosoftTenant;
use release_channel::ReleaseChannel;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// All Maze settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Settings {
    /// Colors: system, light or dark mode, and the theme for each mode.
    pub theme: ThemeSettings,
    /// Base font size of the UI in pixels. Text, spacing, list rows and dialogs scale
    /// with it. Applied between 10 and 32.
    #[schemars(range(min = 10, max = 32))]
    pub font_size: u32,
    /// Keyboard shortcuts.
    pub keymap: KeymapSettings,
    /// Window layout.
    pub layout: Layout,
    /// The message list.
    pub thread_list: ThreadListSettings,
    /// The folder pane.
    pub folder_pane: FolderPaneSettings,
    /// The message view.
    pub reader: ReaderSettings,
    /// The folder the save dialog starts in when you save an attachment, e.g. `/tmp` or
    /// `~/Attachments`. `null`: Downloads in your home folder. A relative path or a
    /// folder that doesn't exist falls back to Downloads.
    pub download_dir: Option<String>,
    /// Writing mail.
    pub compose: ComposeSettings,
    /// Spell checking in the composer: a list of languages as BCP 47 tags (`de-DE`,
    /// `de-AT`, `de-CH`, `en-US`, `en-GB`), `[]` for the system's preferred languages,
    /// or `false` to turn it off.
    pub spellcheck: Spellcheck,
    /// Which messages are kept on this computer (default for all accounts).
    pub offline: OfflinePolicy,
    /// Days of message bodies kept when `offline` is `recent`.
    pub offline_recent_days: u32,
    /// Sync the Trash folders in the background too. Off: a Trash is synced only while
    /// it is open. Can be overridden per account; applies after a restart.
    pub sync_trash: bool,
    /// Overrides per account, keyed by account name.
    pub accounts: BTreeMap<String, AccountOverrides>,
    /// Which domains Maze fetches a favicon for, from the domain's own website (one
    /// request per domain, at most every 30 days; never a third-party icon service).
    pub favicons: FaviconPolicy,
    /// OAuth client registrations for "Sign in with Google" and "Sign in with
    /// Microsoft".
    pub auth: AuthSettings,
    /// AI agents.
    pub agent: AgentSettings,
    /// The sandbox every agent process runs in.
    pub agent_sandbox: AgentSandboxSettings,
    /// Maze Magic: an agent improves the draft in the composer (⌘M / Ctrl+M).
    pub maze_magic: MazeMagicSettings,
    /// Updates of Maze: look for updates in the background, the channel and what
    /// happens when one is found.
    pub update: UpdateSettings,
    /// Notifications for new mail.
    pub notifications: NotificationSettings,
    /// Contacts synced from Google and Microsoft accounts that connected their contacts.
    pub contacts: ContactsSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeSettings::default(),
            font_size: 14,
            keymap: KeymapSettings::default(),
            layout: Layout::default(),
            thread_list: ThreadListSettings::default(),
            folder_pane: FolderPaneSettings::default(),
            reader: ReaderSettings::default(),
            download_dir: None,
            compose: ComposeSettings::default(),
            spellcheck: Spellcheck::default(),
            offline: OfflinePolicy::default(),
            offline_recent_days: 90,
            sync_trash: false,
            accounts: BTreeMap::new(),
            favicons: FaviconPolicy::default(),
            auth: AuthSettings::default(),
            agent: AgentSettings::default(),
            agent_sandbox: AgentSandboxSettings::default(),
            maze_magic: MazeMagicSettings::default(),
            update: UpdateSettings::default(),
            notifications: NotificationSettings::default(),
            contacts: ContactsSettings::default(),
        }
    }
}

impl Settings {
    /// `download_dir` as a path, with a leading `~` meaning `home`. `None` when it is
    /// unset, empty or relative (or starts with `~` and there is no home folder).
    pub fn download_dir_path(&self, home: Option<&Path>) -> Option<PathBuf> {
        let value = self.download_dir.as_deref()?.trim();
        let path = if value == "~" {
            home?.to_path_buf()
        } else if let Some(rest) = value.strip_prefix("~/") {
            home?.join(rest)
        } else {
            PathBuf::from(value)
        };
        path.is_absolute().then_some(path)
    }

    /// The settings that apply to one account: its overrides over the global values.
    pub fn account(&self, name: &str) -> AccountSettings {
        let overrides = self.accounts.get(name);
        AccountSettings {
            offline: overrides.and_then(|o| o.offline).unwrap_or(self.offline),
            offline_recent_days: overrides
                .and_then(|o| o.offline_recent_days)
                .unwrap_or(self.offline_recent_days),
            sync_trash: overrides
                .and_then(|o| o.sync_trash)
                .unwrap_or(self.sync_trash),
            remote_content: overrides
                .and_then(|o| o.remote_content)
                .unwrap_or(self.reader.remote_content),
            contacts_sync: overrides
                .and_then(|o| o.contacts_sync)
                .unwrap_or(self.contacts.sync),
        }
    }

    /// The mailboxes whose shared contacts the account `name` syncs
    /// (`accounts.<name>.shared_contacts`), trimmed, without empty entries.
    pub fn shared_contacts(&self, name: &str) -> Vec<String> {
        self.accounts
            .get(name)
            .map(|o| {
                o.shared_contacts
                    .iter()
                    .map(|m| m.trim().to_owned())
                    .filter(|m| !m.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The footer and the "Sent with Maze" default of one account: its overrides over
    /// `compose.footer` and `compose.support_maze`.
    pub fn footer_for(&self, name: &str) -> FooterSettings {
        let overrides = self.accounts.get(name);
        FooterSettings {
            footer: overrides
                .and_then(|o| o.footer.as_deref())
                .unwrap_or(&self.compose.footer)
                .trim()
                .to_owned(),
            support_maze: overrides
                .and_then(|o| o.support_maze)
                .unwrap_or(self.compose.support_maze),
        }
    }

    /// The aliases of the account named `name` (`accounts.<name>.aliases`):
    /// trimmed, without entries that aren't addresses, each address once.
    pub fn aliases(&self, name: &str) -> Vec<Alias> {
        let Some(overrides) = self.accounts.get(name) else {
            return Vec::new();
        };
        let mut out: Vec<Alias> = Vec::new();
        for entry in &overrides.aliases {
            let (email, name, reply_to) = match entry {
                AliasSetting::Address(email) => (email.as_str(), None, None),
                AliasSetting::Full {
                    email,
                    name,
                    reply_to,
                } => (email.as_str(), name.as_deref(), reply_to.as_deref()),
            };
            let email = email.trim();
            let valid = email.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty() && domain.contains('.') && !email.contains(char::is_whitespace)
            });
            if !valid || out.iter().any(|a| a.email.eq_ignore_ascii_case(email)) {
                continue;
            }
            let clean = |text: Option<&str>| {
                text.map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
            };
            out.push(Alias {
                email: email.to_owned(),
                name: clean(name),
                reply_to: clean(reply_to),
            });
        }
        out
    }

    /// Whether messages sent from the account named `name` get a copy in its Inbox:
    /// its override over `compose.copy_sent_to_inbox`.
    pub fn copy_sent_to_inbox(&self, name: &str) -> bool {
        self.accounts
            .get(name)
            .and_then(|o| o.copy_sent_to_inbox)
            .unwrap_or(self.compose.copy_sent_to_inbox)
    }
}

/// Theme selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ThemeSettings {
    /// Follow the system appearance, or force light or dark.
    pub mode: ThemeMode,
    /// The theme used in light mode.
    pub light: String,
    /// The theme used in dark mode.
    pub dark: String,
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            mode: ThemeMode::System,
            light: "One Light".to_owned(),
            dark: "One Dark".to_owned(),
        }
    }
}

/// Keyboard shortcuts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct KeymapSettings {
    /// The key shortcuts written with `cmd` use. Applies after a restart.
    pub primary_modifier: PrimaryModifier,
    /// Show each button's shortcut on the button (Reply, Archive, … above a message).
    pub show_on_buttons: bool,
}

impl Default for KeymapSettings {
    fn default() -> Self {
        Self {
            primary_modifier: PrimaryModifier::default(),
            show_on_buttons: true,
        }
    }
}

/// The key shortcuts written with `cmd` use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryModifier {
    /// The platform's convention: Command on macOS, Ctrl on Linux.
    #[default]
    Platform,
    /// Ctrl on every platform.
    Ctrl,
    /// Command on macOS, the Super (Windows) key on Linux.
    Super,
}

/// Light or dark appearance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Follow the system appearance.
    #[default]
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// Main window layout (CONCEPT §8.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// Folders, list, message and the agent column side by side.
    #[default]
    FourColumn,
    /// Four-column without the agent column (Thunderbird's Vertical View).
    Vertical,
    /// Thunderbird's Classic View: the message under the list.
    Classic,
    /// Thunderbird's Wide View: the list across the full width.
    Wide,
}

/// The message list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ThreadListSettings {
    /// Two-line cards or a table with columns.
    pub view: ThreadListView,
    /// Sort order.
    pub sort: ThreadSort,
    /// Group messages into conversations.
    pub threaded: bool,
}

impl Default for ThreadListSettings {
    fn default() -> Self {
        Self {
            view: ThreadListView::Cards,
            sort: ThreadSort::DateDesc,
            threaded: true,
        }
    }
}

/// How the message list is drawn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadListView {
    /// Two-line rows.
    #[default]
    Cards,
    /// Sortable, resizable columns.
    Table,
}

/// The message list's order until you sort it yourself (Maze then remembers yours): any
/// column, either way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThreadSort {
    /// Newest first (by `Date` header).
    #[default]
    DateDesc,
    /// Oldest first (by `Date` header).
    DateAsc,
    /// Newest received first.
    ReceivedDesc,
    /// Oldest received first.
    ReceivedAsc,
    /// Sender A to Z.
    FromAsc,
    /// Sender Z to A.
    FromDesc,
    /// Subject A to Z (ignoring `Re:`, `Fwd:`, … and case).
    SubjectAsc,
    /// Subject Z to A.
    SubjectDesc,
    /// Largest first.
    SizeDesc,
    /// Smallest first.
    SizeAsc,
    /// Correspondents (sender, or recipients of mail you sent) A to Z.
    CorrespondentsAsc,
    /// Correspondents Z to A.
    CorrespondentsDesc,
    /// Recipients A to Z.
    RecipientsAsc,
    /// Recipients Z to A.
    RecipientsDesc,
    /// Starred first.
    StarDesc,
    /// Starred last.
    StarAsc,
    /// Unread first.
    UnreadDesc,
    /// Unread last.
    UnreadAsc,
    /// With attachments first.
    AttachmentDesc,
    /// With attachments last.
    AttachmentAsc,
    /// Tags A to Z.
    TagsAsc,
    /// Tags Z to A.
    TagsDesc,
    /// Account A to Z.
    AccountAsc,
    /// Account Z to A.
    AccountDesc,
}

/// The folder pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct FolderPaneSettings {
    /// Folder pane modes shown, in order. Several can be combined.
    pub modes: Vec<FolderPaneMode>,
    /// Folders shown under Favorites. Maze adds and removes entries when you use "Add
    /// to Favorites" and "Remove from Favorites"; entries for folders that don't exist
    /// (any more) are kept but not shown. Favorites are checked for new mail every 30
    /// seconds and notify like the Inbox (except Sent).
    pub favorites: Vec<FavoriteFolder>,
}

impl Default for FolderPaneSettings {
    fn default() -> Self {
        Self {
            modes: vec![FolderPaneMode::Favorite, FolderPaneMode::Unified],
            favorites: Vec::new(),
        }
    }
}

/// A favorite folder: which account, and where in its folder tree. Names rather than
/// ids, so a favorite survives restarts and re-syncs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub struct FavoriteFolder {
    /// The account's name, as in `accounts`.
    pub account: String,
    /// The folder's path in the account's folder tree, names joined with `/`, e.g.
    /// `Customers/Acme`.
    pub folder: String,
}

/// A folder pane mode (Thunderbird's folder modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FolderPaneMode {
    /// Every folder of every account.
    All,
    /// Inbox, Sent, Drafts, … across accounts.
    Unified,
    /// Starred folders.
    Favorite,
    /// Folders with unread mail.
    Unread,
    /// Tags as virtual folders.
    Tags,
    /// Recently used folders.
    Recent,
}

/// The message view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ReaderSettings {
    /// How HTML messages are shown in the message pane next to the list.
    pub default_mode: ReaderMode,
    /// How HTML messages are shown in a tab of their own (double-click, `enter`,
    /// `cmd-o`).
    pub tab_mode: ReaderMode,
    /// Remote content policy (can be overridden per account).
    pub remote_content: RemoteContentPolicy,
}

impl Default for ReaderSettings {
    fn default() -> Self {
        Self {
            default_mode: ReaderMode::Reader,
            tab_mode: ReaderMode::Original,
            remote_content: RemoteContentPolicy::default(),
        }
    }
}

/// Which domains get a favicon, fetched from the domain's own website.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FaviconPolicy {
    /// Domains of your contacts: in an address book, or among the addresses you wrote
    /// to.
    #[default]
    Contacts,
    /// Also domains you only received mail from (tells every sender's website that
    /// Maze looked).
    All,
    /// No favicons are fetched; stored ones aren't shown.
    Off,
}

/// How a message body is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReaderMode {
    /// Sanitized HTML in the native reader (theme colors, fast, no web layout).
    #[default]
    Reader,
    /// The sanitized HTML laid out by the embedded engine as the sender designed it
    /// (ADR-020). Plain-text mail is always shown in the reader.
    Original,
    /// The plain-text part (or text converted from HTML).
    PlainText,
}

/// Remote content (images, fonts, tracking pixels) in messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteContentPolicy {
    /// Blocked, with a banner to allow it per message, sender or domain.
    #[default]
    Ask,
    /// Always blocked, without a banner.
    Never,
    /// Always loaded.
    Always,
}

/// Writing mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ComposeSettings {
    /// Added below every message you send (Markdown), after your signature and above
    /// a quoted message. Empty: none. Can be overridden per account.
    pub footer: String,
    /// Add "Sent with Maze (https://mazemail.dev)" below your messages (the default of
    /// the composer's checkbox). Can be overridden per account.
    pub support_maze: bool,
    /// Also put a copy of every message you send into the account's Inbox (marked
    /// read), next to the Sent copy. Can be overridden per account.
    pub copy_sent_to_inbox: bool,
}

impl Default for ComposeSettings {
    fn default() -> Self {
        Self {
            footer: String::new(),
            support_maze: true,
            copy_sent_to_inbox: false,
        }
    }
}

/// What is added below the messages of one account, see [`Settings::footer_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterSettings {
    /// The footer (Markdown), trimmed; empty for none.
    pub footer: String,
    /// Whether new messages add "Sent with Maze".
    pub support_maze: bool,
}

/// The `spellcheck` setting: `false`, or the languages to check (PLAN-spellcheck).
///
/// In the file it is `false` (off), a list of BCP 47 tags (`["de-AT", "en-US"]`, best
/// first: a German paragraph is checked as the first German language listed), or `[]`
/// (also `true`): the system's preferred languages that have a bundled dictionary,
/// en-US if none has. Tags without a dictionary are ignored with a warning
/// ([`DiagnosticKind::UnsupportedValue`](crate::DiagnosticKind::UnsupportedValue));
/// when none is left, the system's languages apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spellcheck {
    /// `false`: no spell checking.
    Off,
    /// The languages as written in the file (empty: the system's languages).
    Languages(Vec<String>),
}

impl Default for Spellcheck {
    fn default() -> Self {
        Self::Languages(Vec::new())
    }
}

impl Spellcheck {
    /// The bundled languages the setting names, in order, without duplicates; tags
    /// without a dictionary are left out. `None` when spell checking is off; empty when
    /// the system's languages apply (see [`Spellcheck::resolve`]).
    pub fn languages(&self) -> Option<Vec<spelling::Lang>> {
        let Self::Languages(tags) = self else {
            return None;
        };
        let mut langs = Vec::new();
        for tag in tags {
            if let Ok(lang) = tag.parse::<spelling::Lang>()
                && !langs.contains(&lang)
            {
                langs.push(lang);
            }
        }
        Some(langs)
    }

    /// The languages to check: the ones the setting names, or else the bundled ones
    /// matching the system's preferred `system_locales` (best first; en-US if none
    /// matches). Empty when spell checking is off.
    pub fn resolve<S: AsRef<str>>(
        &self,
        system_locales: impl IntoIterator<Item = S>,
    ) -> Vec<spelling::Lang> {
        match self.languages() {
            None => Vec::new(),
            Some(langs) if langs.is_empty() => spelling::Lang::system_languages(system_locales),
            Some(langs) => langs,
        }
    }

    /// The tags without a bundled dictionary, with their index in the list.
    pub(crate) fn unsupported(&self) -> Vec<(usize, &str)> {
        match self {
            Self::Off => Vec::new(),
            Self::Languages(tags) => tags
                .iter()
                .enumerate()
                .filter(|(_, tag)| tag.parse::<spelling::Lang>().is_err())
                .map(|(i, tag)| (i, tag.as_str()))
                .collect(),
        }
    }
}

impl Serialize for Spellcheck {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => serializer.serialize_bool(false),
            Self::Languages(tags) => tags.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Spellcheck {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Spellcheck;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("false or a list of language tags such as [\"de-AT\", \"en-US\"]")
            }

            fn visit_bool<E: serde::de::Error>(self, on: bool) -> Result<Spellcheck, E> {
                Ok(if on {
                    Spellcheck::default()
                } else {
                    Spellcheck::Off
                })
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Spellcheck, A::Error> {
                let mut tags = Vec::new();
                while let Some(tag) = seq.next_element::<String>()? {
                    tags.push(tag);
                }
                Ok(Spellcheck::Languages(tags))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl JsonSchema for Spellcheck {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Spellcheck".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let tags: Vec<&str> = spelling::Lang::ALL.iter().map(|l| l.as_str()).collect();
        schemars::json_schema!({
            "anyOf": [
                {
                    "type": "boolean",
                    "description": "false: no spell checking (true: the system's languages)."
                },
                {
                    "type": "array",
                    "description": "Languages as BCP 47 tags, best first. Empty: the system's languages.",
                    "items": {
                        "type": "string",
                        "examples": tags,
                    },
                    "uniqueItems": true,
                }
            ]
        })
    }
}

/// Which messages are kept on this computer (CONCEPT §5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OfflinePolicy {
    /// Headers only; bodies are fetched when opened.
    HeadersOnly,
    /// Bodies of messages dated (`Date:` header, as the list shows it) within the last
    /// `offline_recent_days` days.
    #[default]
    Recent,
    /// Everything (Thunderbird's "keep messages on this computer").
    All,
    /// Nothing ahead of time.
    OnDemand,
}

/// Per-account overrides. Unset keys use the global value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AccountOverrides {
    /// Overrides `offline`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline: Option<OfflinePolicy>,
    /// Overrides `offline_recent_days`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offline_recent_days: Option<u32>,
    /// Overrides `sync_trash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_trash: Option<bool>,
    /// Overrides `reader.remote_content`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_content: Option<RemoteContentPolicy>,
    /// Overrides `compose.footer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<String>,
    /// Overrides `compose.support_maze`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub support_maze: Option<bool>,
    /// Overrides `compose.copy_sent_to_inbox`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub copy_sent_to_inbox: Option<bool>,
    /// Overrides `contacts.sync`: `false` stops syncing this account's contacts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contacts_sync: Option<bool>,
    /// Microsoft accounts: other mailboxes (email addresses) whose shared Contacts
    /// folder Maze syncs, read-only, next to the account's own contacts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shared_contacts: Vec<String>,
    /// More From addresses of the account (aliases), each an identity in the
    /// composer's From: an address, or `{ "email", "name", "reply_to" }`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<AliasSetting>,
}

/// One entry of `accounts.<name>.aliases`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum AliasSetting {
    /// Just the address; the name is the account's.
    Address(String),
    /// The address with its own name and Reply-To.
    Full {
        /// The From address.
        email: String,
        /// The name shown with it; the account's when left out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Where replies go, if not to `email`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
}

/// An alias of an account, see [`Settings::aliases`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    /// The From address, trimmed.
    pub email: String,
    /// The name, if the setting gives one.
    pub name: Option<String>,
    /// The Reply-To address, if the setting gives one.
    pub reply_to: Option<String>,
}

/// The effective settings of one account, see [`Settings::account`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountSettings {
    /// Offline policy.
    pub offline: OfflinePolicy,
    /// Days kept with [`OfflinePolicy::Recent`].
    pub offline_recent_days: u32,
    /// Whether the Trash is synced in the background.
    pub sync_trash: bool,
    /// Remote content policy.
    pub remote_content: RemoteContentPolicy,
    /// Whether the account's contacts are synced (once connected).
    pub contacts_sync: bool,
}

/// AI agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AgentSettings {
    /// The agent used when none is chosen explicitly (a registry id such as
    /// `claude-acp`, the short names `claude`, `codex`, `gemini`, `cursor`, `pi`, or a
    /// key of `custom`).
    pub default: Option<String>,
    /// The profile of the session token agents started by Maze get.
    pub session_profile: SessionProfile,
    /// Your own ACP agents, keyed by an id of your choice.
    pub custom: BTreeMap<String, CustomAgentSettings>,
    /// Extra environment variables per agent id (agents start with an empty
    /// environment), e.g. `"claude-acp": { "ANTHROPIC_MODEL": "…" }`. Plain text: API
    /// keys belong in the agent column's sign-in card (encrypted store).
    pub env: BTreeMap<String, BTreeMap<String, String>>,
    /// Install new versions of installed agents (and Maze's Node runtime for them) in
    /// the background. Running conversations keep their version.
    pub auto_update: bool,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            default: Some("claude".to_owned()),
            session_profile: SessionProfile::default(),
            custom: BTreeMap::new(),
            env: BTreeMap::new(),
            auto_update: true,
        }
    }
}

/// The profile of an agent session's token (CONCEPT §10.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SessionProfile {
    /// Search and read only.
    Read,
    /// Read, write drafts; archive, move, tag and flag become proposals.
    #[default]
    Triage,
    /// Read, write drafts and change mail directly.
    Write,
}

/// An ACP agent that isn't in the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct CustomAgentSettings {
    /// Shown in the agent picker; the key when empty.
    pub name: Option<String>,
    /// The program, an absolute path or a name looked up on `PATH`.
    pub command: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: BTreeMap<String, String>,
}

/// The instructions Maze Magic sends with a draft unless `maze_magic.prompt` says
/// otherwise.
pub const DEFAULT_MAZE_MAGIC_PROMPT: &str = "Improve my draft. Fix spelling, grammar and \
punctuation. Keep my language, tone and meaning, and make it clear, precise and polite. \
Check it against the earlier messages: answer every question I was asked, and make sure \
nothing I promised or was asked for is missing and nothing contradicts the history. Keep \
names, dates, numbers and links exact. Keep my signature unchanged.";

/// Maze Magic: an agent improves the draft in the composer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MazeMagicSettings {
    /// The agent that improves the draft. `null`: `agent.default`.
    pub agent: Option<String>,
    /// What the agent is asked to do with the draft.
    pub prompt: String,
    /// Send the whole conversation the draft replies to, not only the quoted message.
    pub include_thread: bool,
}

impl Default for MazeMagicSettings {
    fn default() -> Self {
        Self {
            agent: None,
            prompt: DEFAULT_MAZE_MAGIC_PROMPT.to_owned(),
            include_thread: true,
        }
    }
}

/// The agent sandbox (PLAN §4.18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AgentSandboxSettings {
    /// Sandbox agent processes. Turning this off lets agents read your home directory.
    pub enabled: bool,
    /// Extra paths in the home directory agents may read.
    pub allow_read: Vec<String>,
    /// Extra paths outside the home directory agents may never read.
    pub deny_read: Vec<String>,
    /// Additional paths per agent.
    pub agent_paths: BTreeMap<String, AgentPathRules>,
    /// Hosts agents may reach. `null`: any host.
    pub network_hosts: Option<Vec<String>>,
}

impl Default for AgentSandboxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_read: Vec::new(),
            deny_read: Vec::new(),
            agent_paths: BTreeMap::new(),
            network_hosts: None,
        }
    }
}

/// Extra sandbox paths for one agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AgentPathRules {
    /// Extra readable paths.
    pub read: Vec<String>,
    /// Extra writable paths.
    pub write: Vec<String>,
    /// The agent sandboxes itself and can't run inside Maze's sandbox (Seatbelt can't
    /// be nested). Its process then runs unsandboxed; the terminal commands it asks
    /// Maze to run stay sandboxed.
    pub already_sandboxed: bool,
}

/// Updates of Maze (PLAN §4.15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct UpdateSettings {
    /// Look for updates in the background. `false`: only when asked.
    pub auto: bool,
    /// The update channel. `null`: the channel of the installed build.
    pub channel: Option<UpdateChannel>,
    /// What happens when an update is found.
    pub install: UpdateInstall,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto: true,
            channel: None,
            install: UpdateInstall::default(),
        }
    }
}

impl UpdateSettings {
    /// The update channel to follow: the `channel` setting, or the channel of the
    /// running build. Dev builds stay on dev (they never update).
    pub fn effective_channel(&self, build: ReleaseChannel) -> ReleaseChannel {
        match (build, self.channel) {
            (ReleaseChannel::Dev, _) => ReleaseChannel::Dev,
            (_, Some(channel)) => channel.into(),
            (build, None) => build,
        }
    }
}

/// The update channel a user can choose (dev builds never update).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateChannel {
    /// Stable releases.
    Stable,
    /// Prereleases (alpha, beta, rc) and stable releases newer than them.
    Preview,
}

impl From<UpdateChannel> for ReleaseChannel {
    fn from(channel: UpdateChannel) -> Self {
        match channel {
            UpdateChannel::Stable => ReleaseChannel::Stable,
            UpdateChannel::Preview => ReleaseChannel::Preview,
        }
    }
}

/// What happens when an update is found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateInstall {
    /// Download and install in the background, then offer a restart.
    #[default]
    Background,
    /// Only tell me that an update is available.
    NotifyOnly,
}

/// OAuth client registrations (PLAN §4.6). Maze ships no Google client ID until the
/// app is verified (CONCEPT §6): bring your own from the Google Cloud console.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AuthSettings {
    /// Google (Gmail, Google Workspace).
    pub google: GoogleAuthSettings,
    /// Microsoft (Outlook.com, Microsoft 365).
    pub microsoft: MicrosoftAuthSettings,
}

/// A Google OAuth client of type "Desktop app".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct GoogleAuthSettings {
    /// The OAuth client ID (`….apps.googleusercontent.com`). `null`: "Sign in with
    /// Google" is unavailable.
    pub client_id: Option<String>,
    /// The client secret Google issues to desktop clients. It ships with every copy
    /// of an app, so it isn't really secret, but Google requires it.
    pub client_secret: Option<String>,
}

/// A Microsoft Entra app registration (public client, no secret).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MicrosoftAuthSettings {
    /// The application (client) ID. `null`: "Sign in with Microsoft" is unavailable
    /// until Maze's own registration is configured.
    pub client_id: Option<String>,
    /// The directory new sign-ins use: "common" (work, school and personal
    /// accounts), "organizations" (work and school accounts), "consumers" (personal
    /// accounts), a tenant ID (a GUID) or a verified domain ("contoso.com"). A
    /// single-tenant app registration needs its tenant ID or domain. Accounts keep
    /// the tenant they signed in with.
    #[schemars(with = "String")]
    pub tenant: MicrosoftTenant,
}

/// Contacts synced from the accounts' servers (ADR-024).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ContactsSettings {
    /// Sync the contacts of accounts that connected them. `false` keeps the copy on
    /// this computer but stops syncing it.
    pub sync: bool,
    /// Minutes between contact syncs (also at startup and on demand). At least 5.
    #[schemars(range(min = 5))]
    pub sync_interval_minutes: u32,
    /// Pictures from Gravatar for contacts without a photo of their own (ADR-032).
    /// Off by default: Gravatar learns a hash of each address it is asked about.
    pub gravatar: GravatarPolicy,
}

impl Default for ContactsSettings {
    fn default() -> Self {
        Self {
            sync: true,
            sync_interval_minutes: 30,
            gravatar: GravatarPolicy::default(),
        }
    }
}

/// Which addresses Maze asks Gravatar for a picture (ADR-032).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GravatarPolicy {
    /// None: Gravatar is never asked, and pictures it gave aren't shown.
    #[default]
    Off,
    /// Your contacts without a photo of their own: in an address book, or among the
    /// addresses you wrote to. Gravatar learns a hash of each of these addresses.
    Contacts,
}

/// Notifications for new mail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct NotificationSettings {
    /// Show notifications.
    pub enabled: bool,
    /// What a notification shows. macOS keeps delivered notification text in its own
    /// database.
    pub content: NotificationContent,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            content: NotificationContent::SenderAndSubject,
        }
    }
}

/// What a new-mail notification shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationContent {
    /// Sender and subject (like Thunderbird).
    #[default]
    SenderAndSubject,
    /// The sender only.
    SenderOnly,
    /// Only that new mail arrived.
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_update_channel_follows_the_build() {
        let mut settings = UpdateSettings::default();
        assert_eq!(
            settings.effective_channel(ReleaseChannel::Preview),
            ReleaseChannel::Preview
        );
        assert_eq!(
            settings.effective_channel(ReleaseChannel::Stable),
            ReleaseChannel::Stable
        );
        settings.channel = Some(UpdateChannel::Preview);
        assert_eq!(
            settings.effective_channel(ReleaseChannel::Stable),
            ReleaseChannel::Preview
        );
        assert_eq!(
            settings.effective_channel(ReleaseChannel::Dev),
            ReleaseChannel::Dev
        );
    }

    #[test]
    fn the_download_dir_expands_the_home_folder_and_ignores_relative_paths() {
        let home = Path::new("/home/anna");
        let dir = |value: Option<&str>| {
            let settings = Settings {
                download_dir: value.map(str::to_owned),
                ..Settings::default()
            };
            settings.download_dir_path(Some(home))
        };
        assert_eq!(dir(None), None, "unset: Downloads");
        assert_eq!(dir(Some("/tmp")), Some(PathBuf::from("/tmp")));
        assert_eq!(dir(Some(" /tmp ")), Some(PathBuf::from("/tmp")));
        assert_eq!(dir(Some("~")), Some(PathBuf::from("/home/anna")));
        assert_eq!(
            dir(Some("~/Attachments")),
            Some(PathBuf::from("/home/anna/Attachments"))
        );
        assert_eq!(dir(Some("")), None);
        assert_eq!(dir(Some("Attachments")), None, "relative to what?");
        assert_eq!(
            dir(Some("~anna/x")),
            None,
            "other users' homes aren't expanded"
        );
        let tilde = Settings {
            download_dir: Some("~/Attachments".to_owned()),
            ..Settings::default()
        };
        assert_eq!(tilde.download_dir_path(None), None, "no home folder");
    }

    #[test]
    fn oauth_client_ids_are_unset_by_default_and_parse() {
        let defaults = Settings::default();
        assert_eq!(defaults.auth.google.client_id, None);
        assert_eq!(defaults.auth.microsoft.client_id, None);
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "auth": {
                "google": { "client_id": "id.apps.googleusercontent.com", "client_secret": "s" },
                "microsoft": { "client_id": "entra-app" }
            }
        }))
        .unwrap();
        assert_eq!(
            settings.auth.google.client_id.as_deref(),
            Some("id.apps.googleusercontent.com")
        );
        assert_eq!(settings.auth.google.client_secret.as_deref(), Some("s"));
        assert_eq!(
            settings.auth.microsoft.client_id.as_deref(),
            Some("entra-app")
        );
        assert!(settings.auth.microsoft.tenant.is_common(), "the default");
    }

    #[test]
    fn the_microsoft_tenant_is_validated() {
        let loaded = crate::parse_settings(Some(
            r#"{ "auth": { "microsoft": { "tenant": "Contoso.com" } } }"#,
        ));
        assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
        assert_eq!(
            loaded.settings.auth.microsoft.tenant.as_str(),
            "contoso.com"
        );

        let loaded = crate::parse_settings(Some(
            r#"{ "auth": { "microsoft": { "tenant": "https://login.microsoftonline.com/x" } } }"#,
        ));
        assert_eq!(loaded.diagnostics.len(), 1, "{:?}", loaded.diagnostics);
        let diagnostic = &loaded.diagnostics[0];
        assert_eq!(diagnostic.kind, crate::DiagnosticKind::InvalidValue);
        assert_eq!(diagnostic.path, "auth.microsoft.tenant");
        assert!(
            diagnostic.message.contains("not a Microsoft tenant"),
            "{diagnostic}"
        );
        assert!(
            loaded.settings.auth.microsoft.tenant.is_common(),
            "the default"
        );
    }

    #[test]
    fn account_overrides_fall_back_to_globals() {
        let mut settings = Settings::default();
        settings.accounts.insert(
            "work".to_owned(),
            AccountOverrides {
                offline: Some(OfflinePolicy::All),
                remote_content: Some(RemoteContentPolicy::Never),
                ..AccountOverrides::default()
            },
        );

        let work = settings.account("work");
        assert_eq!(work.offline, OfflinePolicy::All);
        assert_eq!(work.offline_recent_days, 90);
        assert_eq!(work.remote_content, RemoteContentPolicy::Never);

        let other = settings.account("personal");
        assert_eq!(other.offline, OfflinePolicy::Recent);
        assert_eq!(other.remote_content, RemoteContentPolicy::Ask);
    }

    #[test]
    fn spellcheck_is_false_or_a_list_of_languages() {
        use spelling::Lang;
        let parse = |json: &str| serde_json::from_str::<Spellcheck>(json).unwrap();
        assert_eq!(parse("false"), Spellcheck::Off);
        assert_eq!(parse("true"), Spellcheck::default());
        assert_eq!(parse("[]"), Spellcheck::Languages(Vec::new()));
        assert_eq!(
            parse(r#"["de-AT", "en-US"]"#),
            Spellcheck::Languages(vec!["de-AT".into(), "en-US".into()])
        );
        assert!(serde_json::from_str::<Spellcheck>(r#""de-AT""#).is_err());
        assert!(serde_json::from_str::<Spellcheck>("[1]").is_err());
        assert_eq!(serde_json::to_string(&Spellcheck::Off).unwrap(), "false");
        assert_eq!(
            serde_json::to_string(&parse(r#"["de-AT"]"#)).unwrap(),
            r#"["de-AT"]"#
        );

        let system = ["de-AT", "en-AT"];
        assert_eq!(Spellcheck::Off.resolve(system), Vec::<Lang>::new());
        assert_eq!(parse("[]").resolve(system), [Lang::DeAt, Lang::EnUs]);
        assert_eq!(parse("[]").resolve(["fr-FR"]), [Lang::EnUs]);
        let listed = parse(r#"["en-GB", "fr-FR", "de_CH", "en-GB"]"#);
        assert_eq!(listed.resolve(system), [Lang::EnGb, Lang::DeCh]);
        assert_eq!(listed.unsupported(), [(1, "fr-FR")]);
        // Only unknown tags: the system's languages.
        assert_eq!(
            parse(r#"["fr-FR"]"#).resolve(system),
            [Lang::DeAt, Lang::EnUs]
        );
    }

    #[test]
    fn footers_per_account() {
        let mut settings = Settings::default();
        assert_eq!(
            settings.footer_for("work"),
            FooterSettings {
                footer: String::new(),
                support_maze: true,
            }
        );
        settings.compose.footer = "  Philip · *Maze*\n".to_owned();
        settings.accounts.insert(
            "work".to_owned(),
            AccountOverrides {
                footer: Some("Acme Inc., Vienna".to_owned()),
                support_maze: Some(false),
                ..AccountOverrides::default()
            },
        );
        let work = settings.footer_for("work");
        assert_eq!(work.footer, "Acme Inc., Vienna");
        assert!(!work.support_maze);
        let personal = settings.footer_for("personal");
        assert_eq!(personal.footer, "Philip · *Maze*");
        assert!(personal.support_maze);
    }

    #[test]
    fn aliases_are_addresses_or_objects_each_once() {
        let settings: Settings = serde_json::from_str(
            r#"{ "accounts": { "work": { "aliases": [
                "sales@contoso.com",
                { "email": " support@contoso.com ", "name": "Contoso Support",
                  "reply_to": "help@contoso.com" },
                "SALES@contoso.com",
                "not an address",
                { "email": "info@contoso.com", "name": "  " }
            ] } } }"#,
        )
        .unwrap();
        let aliases = settings.aliases("work");
        let emails: Vec<&str> = aliases.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(
            emails,
            [
                "sales@contoso.com",
                "support@contoso.com",
                "info@contoso.com"
            ]
        );
        assert_eq!(aliases[0].name, None);
        assert_eq!(aliases[1].name.as_deref(), Some("Contoso Support"));
        assert_eq!(aliases[1].reply_to.as_deref(), Some("help@contoso.com"));
        assert_eq!(aliases[2].name, None, "a blank name is no name");
        assert!(settings.aliases("personal").is_empty());
    }

    #[test]
    fn copies_of_sent_mail_in_the_inbox_per_account() {
        let mut settings = Settings::default();
        assert!(!settings.copy_sent_to_inbox("work"), "off by default");
        settings.compose.copy_sent_to_inbox = true;
        settings.accounts.insert(
            "work".to_owned(),
            AccountOverrides {
                copy_sent_to_inbox: Some(false),
                ..AccountOverrides::default()
            },
        );
        assert!(!settings.copy_sent_to_inbox("work"));
        assert!(settings.copy_sent_to_inbox("personal"));
    }
}

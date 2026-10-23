//! The typed settings (CONCEPT §16, PLAN §4.9, §4.15, §4.18).
//!
//! Every struct has `#[serde(default)]` and a [`Default`] impl that matches
//! `assets/settings/default.jsonc` (a test keeps the two in sync). The defaults file is
//! what people read; the `Default` impls are the fallback when a value can't be used.
//! Doc comments become descriptions in the generated JSON Schema.

use std::collections::BTreeMap;

use release_channel::ReleaseChannel;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// All Maze settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Settings {
    /// Colors: system, light or dark mode, and the theme for each mode.
    pub theme: ThemeSettings,
    /// Window layout.
    pub layout: Layout,
    /// Below this window width (px) the message moves under the list (Classic layout).
    pub narrow_layout_below_px: u32,
    /// The agent column.
    pub agent_panel: AgentPanelSettings,
    /// The message list.
    pub thread_list: ThreadListSettings,
    /// The folder pane.
    pub folder_pane: FolderPaneSettings,
    /// Quick Filter.
    pub quick_filter: QuickFilterSettings,
    /// The message view.
    pub reader: ReaderSettings,
    /// Writing mail.
    pub compose: ComposeSettings,
    /// Spell-check languages as BCP 47 tags. Empty: the system's languages.
    pub spellcheck: Vec<String>,
    /// Which messages are kept on this computer (default for all accounts).
    pub offline: OfflinePolicy,
    /// Days of message bodies kept when `offline` is `recent`.
    pub offline_recent_days: u32,
    /// Overrides per account, keyed by account label.
    pub accounts: BTreeMap<String, AccountOverrides>,
    /// OAuth client registrations for "Sign in with Google" and "Sign in with
    /// Microsoft".
    pub auth: AuthSettings,
    /// AI agents.
    pub agent: AgentSettings,
    /// The sandbox every agent process runs in.
    pub agent_sandbox: AgentSandboxSettings,
    /// Look for updates in the background. `false`: only when asked.
    pub auto_update: bool,
    /// The update channel. `null`: the channel of the installed build.
    pub update_channel: Option<UpdateChannel>,
    /// What happens when an update is found.
    pub auto_update_install: AutoUpdateInstall,
    /// Notifications for new mail.
    pub notifications: NotificationSettings,
    /// Log verbosity. The `MAZEMAIL_LOG` environment variable takes precedence.
    pub log_level: LogLevel,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeSettings::default(),
            layout: Layout::default(),
            narrow_layout_below_px: 1000,
            agent_panel: AgentPanelSettings::default(),
            thread_list: ThreadListSettings::default(),
            folder_pane: FolderPaneSettings::default(),
            quick_filter: QuickFilterSettings::default(),
            reader: ReaderSettings::default(),
            compose: ComposeSettings::default(),
            spellcheck: Vec::new(),
            offline: OfflinePolicy::default(),
            offline_recent_days: 90,
            accounts: BTreeMap::new(),
            auth: AuthSettings::default(),
            agent: AgentSettings::default(),
            agent_sandbox: AgentSandboxSettings::default(),
            auto_update: true,
            update_channel: None,
            auto_update_install: AutoUpdateInstall::default(),
            notifications: NotificationSettings::default(),
            log_level: LogLevel::default(),
        }
    }
}

impl Settings {
    /// The update channel to follow: the `update_channel` setting, or the channel of
    /// the running build. Dev builds stay on dev (they never update).
    pub fn effective_update_channel(&self, build: ReleaseChannel) -> ReleaseChannel {
        match (build, self.update_channel) {
            (ReleaseChannel::Dev, _) => ReleaseChannel::Dev,
            (_, Some(channel)) => channel.into(),
            (build, None) => build,
        }
    }

    /// The settings that apply to one account: its overrides over the global values.
    pub fn account(&self, label: &str) -> AccountSettings {
        let overrides = self.accounts.get(label);
        AccountSettings {
            offline: overrides.and_then(|o| o.offline).unwrap_or(self.offline),
            offline_recent_days: overrides
                .and_then(|o| o.offline_recent_days)
                .unwrap_or(self.offline_recent_days),
            remote_content: overrides
                .and_then(|o| o.remote_content)
                .unwrap_or(self.reader.remote_content),
            agent_access: self.agent.access_for(label),
        }
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

/// The agent column.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AgentPanelSettings {
    /// Show the agent column.
    pub visible: bool,
    /// Width in px.
    pub width: u32,
    /// Show the agent for the selected message or thread.
    pub follow_selection: bool,
    /// Below this window width (px) the column collapses to a thin strip.
    pub collapse_below_px: u32,
}

impl Default for AgentPanelSettings {
    fn default() -> Self {
        Self {
            visible: true,
            width: 380,
            follow_selection: true,
            collapse_below_px: 1400,
        }
    }
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
    /// (any more) are kept but not shown.
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
    /// The account's label, as in `accounts`.
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

/// Quick Filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct QuickFilterSettings {
    /// How several active filters combine.
    pub boolean_mode: BooleanMode,
}

/// How filters combine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BooleanMode {
    /// A message matches if any filter matches.
    #[default]
    Or,
    /// A message matches only if all filters match.
    And,
}

/// The message view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ReaderSettings {
    /// How messages are shown by default.
    pub default_mode: ReaderMode,
    /// Remote content policy (can be overridden per account).
    pub remote_content: RemoteContentPolicy,
}

/// How a message body is shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReaderMode {
    /// Sanitized HTML in the native reader.
    #[default]
    Reader,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ComposeSettings {
    /// Where the composer opens.
    pub open_in: ComposeOpenIn,
    /// The editing format.
    pub format: ComposeFormat,
    /// Seconds during which a sent message can be undone (0 to 30).
    pub undo_send_seconds: UndoSendSeconds,
}

/// Where the composer opens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposeOpenIn {
    /// A tab in the main window (Zed style).
    #[default]
    Tab,
    /// A separate window (Thunderbird style).
    Window,
}

/// The composer's editing format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposeFormat {
    /// Markdown, sent as HTML plus a plain-text part.
    #[default]
    Markdown,
    /// Plain text only.
    Plain,
}

/// The undo-send delay in seconds, at most [`UndoSendSeconds::MAX`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct UndoSendSeconds(u8);

impl UndoSendSeconds {
    /// The longest allowed delay.
    pub const MAX: u8 = 30;

    /// Creates a delay, or `None` if it exceeds [`Self::MAX`].
    pub const fn new(seconds: u8) -> Option<Self> {
        if seconds <= Self::MAX {
            Some(Self(seconds))
        } else {
            None
        }
    }

    /// The delay in seconds.
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Default for UndoSendSeconds {
    fn default() -> Self {
        Self(10)
    }
}

impl TryFrom<u32> for UndoSendSeconds {
    type Error = String;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        u8::try_from(value)
            .ok()
            .and_then(Self::new)
            .ok_or_else(|| format!("{value} is out of range (0 to {})", Self::MAX))
    }
}

impl From<UndoSendSeconds> for u32 {
    fn from(value: UndoSendSeconds) -> Self {
        u32::from(value.0)
    }
}

impl JsonSchema for UndoSendSeconds {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "UndoSendSeconds".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "integer",
            "minimum": 0,
            "maximum": Self::MAX,
        })
    }
}

/// Which messages are kept on this computer (CONCEPT §5.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OfflinePolicy {
    /// Headers only; bodies are fetched when opened.
    HeadersOnly,
    /// Bodies of the last `offline_recent_days` days.
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
    /// Overrides `reader.remote_content`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_content: Option<RemoteContentPolicy>,
}

/// The effective settings of one account, see [`Settings::account`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountSettings {
    /// Offline policy.
    pub offline: OfflinePolicy,
    /// Days kept with [`OfflinePolicy::Recent`].
    pub offline_recent_days: u32,
    /// Remote content policy.
    pub remote_content: RemoteContentPolicy,
    /// What agents may do with this account.
    pub agent_access: AgentAccess,
}

/// AI agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AgentSettings {
    /// The agent used when none is chosen explicitly.
    pub default: Option<String>,
    /// Access per account label. Accounts not listed are `off`.
    pub accounts: BTreeMap<String, AgentAccess>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            default: Some("claude".to_owned()),
            accounts: BTreeMap::new(),
        }
    }
}

impl AgentSettings {
    /// The access agents have to an account (off unless listed).
    pub fn access_for(&self, label: &str) -> AgentAccess {
        self.accounts.get(label).copied().unwrap_or_default()
    }
}

/// What agents may do with an account (CONCEPT §10.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentAccess {
    /// No agent sees this account.
    #[default]
    Off,
    /// Only agents backed by local models.
    LocalOnly,
    /// Any agent.
    Any,
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
pub enum AutoUpdateInstall {
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
    /// Show the unread count on the dock icon.
    pub dock_badge: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            content: NotificationContent::SenderAndSubject,
            dock_badge: true,
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

/// Log verbosity.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Errors and warnings.
    Warn,
    /// Normal operation.
    #[default]
    Info,
    /// Detailed diagnostics.
    Debug,
    /// Everything.
    Trace,
}

impl LogLevel {
    /// The level as a `tracing` filter directive (`"info"`, …).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_send_seconds_range() {
        assert_eq!(UndoSendSeconds::try_from(0).unwrap().get(), 0);
        assert_eq!(UndoSendSeconds::try_from(30).unwrap().get(), 30);
        assert!(UndoSendSeconds::try_from(31).is_err());
        assert!(UndoSendSeconds::try_from(300).is_err());
        assert_eq!(UndoSendSeconds::default().get(), 10);
    }

    #[test]
    fn effective_update_channel() {
        let mut settings = Settings::default();
        assert_eq!(
            settings.effective_update_channel(ReleaseChannel::Preview),
            ReleaseChannel::Preview
        );
        assert_eq!(
            settings.effective_update_channel(ReleaseChannel::Stable),
            ReleaseChannel::Stable
        );
        settings.update_channel = Some(UpdateChannel::Preview);
        assert_eq!(
            settings.effective_update_channel(ReleaseChannel::Stable),
            ReleaseChannel::Preview
        );
        assert_eq!(
            settings.effective_update_channel(ReleaseChannel::Dev),
            ReleaseChannel::Dev
        );
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
        settings
            .agent
            .accounts
            .insert("work".to_owned(), AgentAccess::Any);

        let work = settings.account("work");
        assert_eq!(work.offline, OfflinePolicy::All);
        assert_eq!(work.offline_recent_days, 90);
        assert_eq!(work.remote_content, RemoteContentPolicy::Never);
        assert_eq!(work.agent_access, AgentAccess::Any);

        let other = settings.account("personal");
        assert_eq!(other.offline, OfflinePolicy::Recent);
        assert_eq!(other.remote_content, RemoteContentPolicy::Ask);
        assert_eq!(other.agent_access, AgentAccess::Off);
    }
}

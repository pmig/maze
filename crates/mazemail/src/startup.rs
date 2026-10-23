//! Opening the core the way the app and `--headless` do (PLAN §4.17 startup).
//!
//! The core is opened with a [`TokenAuthorizer`], so agent commands that reach
//! [`CoreHandle::apply`](mazemail_core::CoreHandle::apply) follow their token's
//! profile, and the same authorizer goes into [`api::services`] for the local API.

use std::sync::Arc;

use local_api::{DispatchConfig, ServerConfig};
use mail_ui::source::ListSort;
use mail_ui::{ListDefaults, ListMode, UI_STATE_FILE, UiStateStore};
use mazemail_core::api::TokenAuthorizer;
use mazemail_core::{CoreConfig, Error, SyncEngineConfig};
use mazemail_types::PRODUCT_NAME;
use settings::{Paths, ThreadListSettings, ThreadListView, ThreadSort};
use time::UtcOffset;
use vault::KeyStore;

/// The core configuration for the standard directories: data directory and
/// `settings.json` from `paths`, the version from `version.txt`, the user's time zone,
/// the token authorizer, and the sync engine with the account secrets in `secrets`
/// (the keychain, see [`crate::keys::secret_store`]).
pub fn core_config(
    paths: &Paths,
    key_store: Arc<dyn KeyStore>,
    secrets: Arc<dyn auth::SecretStore>,
    authorizer: &TokenAuthorizer,
    utc_offset: UtcOffset,
) -> CoreConfig {
    let mut config = CoreConfig::from_paths(paths, key_store)
        .with_authorizer(Arc::new(authorizer.clone()))
        .with_utc_offset(utc_offset)
        .with_sync_engine(SyncEngineConfig::new(secrets));
    config.app_version = release_channel::VERSION.to_owned();
    config
}

/// The local API server configuration: defaults, with Maze's version for `status`.
pub fn server_config() -> ServerConfig {
    ServerConfig {
        dispatch: DispatchConfig {
            app_version: release_channel::VERSION.to_owned(),
            ..DispatchConfig::default()
        },
        ..ServerConfig::default()
    }
}

/// A one-line explanation of why the core didn't open, for dialogs and the terminal.
/// Never contains message content (the core's errors don't carry any).
pub fn describe_open_error(error: &Error) -> String {
    match error {
        Error::AlreadyRunning { data_dir, pid } => {
            let by = pid
                .map(|pid| format!(" (process {pid})"))
                .unwrap_or_default();
            format!(
                "{PRODUCT_NAME} is already running{by} with the data directory {}. Switch to \
                 the running {PRODUCT_NAME}, or quit it first.",
                data_dir.display()
            )
        }
        Error::MissingKeyForExistingData(lost) => format!(
            "The mail data in {} is encrypted, but its key is missing from the key store. \
             Start {PRODUCT_NAME} (the app) to enter your recovery key or to start fresh.",
            lost.data_dir().display()
        ),
        other => format!("{PRODUCT_NAME} could not open its data: {other}"),
    }
}

/// The message list's defaults for folders without remembered state, from
/// `thread_list` in `settings.json`. The store sorts by date only, so the other sort
/// orders fall back to newest first.
pub fn list_defaults(settings: &ThreadListSettings) -> ListDefaults {
    ListDefaults {
        mode: match settings.view {
            ThreadListView::Cards => ListMode::Cards,
            ThreadListView::Table => ListMode::Table,
        },
        threaded: settings.threaded,
        sort: match settings.sort {
            ThreadSort::DateAsc | ThreadSort::ReceivedAsc => ListSort::OldestFirst,
            _ => ListSort::NewestFirst,
        },
    }
}

/// The message list view state in the data directory (`ui_state.json`, see
/// [`mail_ui::ui_state`]) over the defaults in `settings.json`.
pub fn ui_state(paths: &Paths) -> UiStateStore {
    let loaded = settings::load_settings(&paths.settings_file());
    UiStateStore::load(
        paths.data_dir().join(UI_STATE_FILE),
        list_defaults(&loaded.settings.thread_list),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_defaults_follow_the_settings() {
        let defaults = list_defaults(&ThreadListSettings::default());
        assert_eq!(defaults, ListDefaults::default());
        let table = list_defaults(&ThreadListSettings {
            view: ThreadListView::Table,
            sort: ThreadSort::DateAsc,
            threaded: false,
        });
        assert_eq!(
            table,
            ListDefaults {
                mode: ListMode::Table,
                threaded: false,
                sort: ListSort::OldestFirst,
            }
        );
        // Sorts the store can't do fall back to the date.
        let by_sender = list_defaults(&ThreadListSettings {
            sort: ThreadSort::FromAsc,
            ..ThreadListSettings::default()
        });
        assert_eq!(by_sender.sort, ListSort::NewestFirst);
    }
}

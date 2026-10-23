//! Opening the core the way the app and `--headless` do (PLAN §4.17 startup).
//!
//! The core is opened with a [`TokenAuthorizer`], so agent commands that reach
//! [`CoreHandle::apply`](mazemail_core::CoreHandle::apply) follow their token's
//! profile, and the same authorizer goes into [`api::services`] for the local API.

use std::sync::Arc;

use local_api::{DispatchConfig, ServerConfig};
use mazemail_core::api::TokenAuthorizer;
use mazemail_core::{CoreConfig, Error};
use mazemail_types::PRODUCT_NAME;
use settings::Paths;
use time::UtcOffset;
use vault::KeyStore;

/// The core configuration for the standard directories: data directory and
/// `settings.json` from `paths`, the version from `version.txt`, the user's time zone
/// and the token authorizer.
pub fn core_config(
    paths: &Paths,
    key_store: Arc<dyn KeyStore>,
    authorizer: &TokenAuthorizer,
    utc_offset: UtcOffset,
) -> CoreConfig {
    let mut config = CoreConfig::from_paths(paths, key_store)
        .with_authorizer(Arc::new(authorizer.clone()))
        .with_utc_offset(utc_offset);
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

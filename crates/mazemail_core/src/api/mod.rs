//! Core's side of the local API (PLAN §4.11, CONCEPT §10.2–10.5): everything the
//! `local_api` socket server needs from core, implemented over [`CoreHandle`].
//!
//! | `local_api` trait | Implementation | Backed by |
//! | ----------------- | -------------- | --------- |
//! | [`CommandHandler`](local_api::CommandHandler) | [`CoreCommandHandler`] | the [`CoreHandle`] queries, [`CoreHandle::apply`], `import` |
//! | [`TokenStore`](local_api::TokenStore) | [`StoreTokenStore`] | the encrypted `api_token` table |
//! | [`AccountPolicy`](local_api::AccountPolicy) | [`SettingsAccountPolicy`] | `agent.accounts` in the settings |
//! | [`AuditSink`](local_api::AuditSink) | [`StoreAuditSink`] | the encrypted `audit_log` |
//!
//! plus core's own permission hook, [`TokenAuthorizer`] (a
//! [`CommandAuthorizer`](crate::CommandAuthorizer)), which makes
//! [`CoreHandle::apply`] follow the token's profile a second time.
//!
//! # Wiring
//!
//! ```no_run
//! # async fn run(key_store: std::sync::Arc<dyn vault::KeyStore>, paths: settings::Paths)
//! # -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//! use mazemail_core::{CoreConfig, CoreHandle, api};
//!
//! // The authorizer exists before the core: `open` takes it in the config.
//! let authorizer = api::TokenAuthorizer::new();
//! let config = CoreConfig::from_paths(&paths, key_store)
//!     .with_authorizer(Arc::new(authorizer.clone()));
//! let core = CoreHandle::open(config).await?;
//! let services = api::services(&core, &authorizer, Arc::new(local_api::DenyAllApprover));
//! let server = local_api::LocalApiServer::bind(&paths, services, Default::default()).await?;
//! server.serve(std::future::pending()).await;
//! # Ok(()) }
//! ```
//!
//! [`TokenAuthorizer`] and [`StoreTokenStore`] share a [`TokenCache`], so a token
//! verified by the server is known to the synchronous authorizer by the time the
//! handler calls `apply`.

mod accounts;
mod audit;
mod export;
mod handler;
mod import;
mod policy;
mod tokens;

use std::sync::Arc;

use local_api::{Approver, Services};

pub use audit::StoreAuditSink;
pub use handler::{CoreCommandHandler, ProposalPublicId, not_available};
pub use policy::SettingsAccountPolicy;
pub use tokens::{StoreTokenStore, TokenAuthorizer, TokenCache};

use crate::handle::CoreHandle;

/// The local API services over `core`: the command handler, token store, audit sink
/// and account policy, sharing `authorizer`'s token cache, with `approver` answering
/// logins and `ask` commands.
///
/// `authorizer` must be the one `core` was opened with
/// ([`CoreConfig::with_authorizer`](crate::CoreConfig::with_authorizer)); otherwise
/// agent commands that reach [`CoreHandle::apply`] are refused (fail closed).
pub fn services(
    core: &CoreHandle,
    authorizer: &TokenAuthorizer,
    approver: Arc<dyn Approver>,
) -> Services {
    let cache = Arc::clone(authorizer.cache());
    Services {
        handler: Arc::new(CoreCommandHandler::new(core.clone(), Arc::clone(&cache))),
        tokens: Arc::new(StoreTokenStore::new(core.clone(), Arc::clone(&cache))),
        approver,
        audit: Arc::new(StoreAuditSink::new(core, cache)),
        policy: Arc::new(SettingsAccountPolicy::new(core.clone())),
    }
}

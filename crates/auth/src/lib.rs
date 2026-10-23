//! Maze account authentication (PLAN §4.6). Layer L1; depends only on
//! `mazemail_types`.
//!
//! - **OAuth2** authorization code flow with PKCE (RFC 7636) and a loopback redirect
//!   on `http://127.0.0.1:<random port>` opened in the system browser, the way
//!   Thunderbird does it ([`OAuthClient`], [`BrowserOpener`]).
//! - **Providers** Google and Microsoft with Thunderbird's endpoints and scopes,
//!   client IDs from settings, and detection from an address or server host
//!   ([`ProviderRegistry`], [`ProviderKind`]).
//! - **Token lifecycle**: access tokens in memory, refreshed 60 s before expiry;
//!   refresh tokens in the keychain ([`OAuthTokenManager`], [`TokenProvider`]).
//! - **Secrets**: passwords and refresh tokens under `mazemail/<account-uuid>/<kind>`
//!   ([`SecretStore`], [`AccountSecrets`]).
//! - **SASL**: XOAUTH2 and OAUTHBEARER initial responses ([`sasl`]).
//!
//! Tokens, passwords, codes and addresses are wrapped in
//! [`Sensitive`](mazemail_types::Sensitive) and never logged.
//!
//! ```no_run
//! # async fn demo() -> Result<(), auth::AuthError> {
//! use std::sync::Arc;
//! use auth::{
//!     AccountSecrets, AuthorizeOptions, ClientCredentials, OAuthTokenManager, ProviderKind,
//!     ProviderRegistry, SystemBrowser, TokenProvider, system_secret_store,
//! };
//!
//! let registry = ProviderRegistry::new()
//!     .with_credentials(ProviderKind::Microsoft, ClientCredentials::new("entra-app-id"));
//! let kind = ProviderKind::detect(Some("bob@outlook.com"), None).unwrap();
//! let account = auth::Uuid::nil();
//! let manager = OAuthTokenManager::new(
//!     registry.client(kind)?,
//!     AccountSecrets::new(system_secret_store(), account),
//! );
//! manager
//!     .sign_in(&SystemBrowser, &AuthorizeOptions::new().login_hint("bob@outlook.com"))
//!     .await?;
//! let token = manager.access_token().await?;
//! # let _ = token; Ok(()) }
//! ```

mod browser;
mod client;
mod error;
mod loopback;
pub mod pkce;
mod provider;
pub mod sasl;
mod secret;
mod token;

pub use browser::{BrowserOpener, SystemBrowser};
pub use client::{AuthorizationRequest, AuthorizeOptions, DEFAULT_AUTHORIZE_TIMEOUT, OAuthClient};
pub use error::{ADMIN_CONSENT_EXPLANATION, AuthError, OAuthErrorDetails, ReauthReason};
pub use provider::{
    ClientCredentials, ParseProviderKindError, Provider, ProviderKind, ProviderRegistry,
};
#[cfg(target_os = "macos")]
pub use secret::KeychainSecretStore;
#[cfg(target_os = "linux")]
pub use secret::SecretServiceStore;
pub use secret::{
    AccountSecrets, FileSecretStore, MemorySecretStore, SERVICE, SecretKey, SecretKind,
    SecretStore, SecretStoreError, system_secret_store,
};
pub use token::{OAuthTokenManager, REFRESH_MARGIN, StaticTokenProvider, TokenProvider, TokenSet};
pub use uuid::Uuid;

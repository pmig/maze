//! Sender verification and phishing heuristics for Maze (PLAN §4.7).
//!
//! This crate is layer L1. It replaces the DKIM Verifier add-on (CONCEPT §11.4)
//! and feeds the verification row of the message header (CONCEPT §8.4).
//!
//! # Verification
//!
//! [`Verifier::verify`] checks one raw message and returns an [`AuthReport`],
//! which the store caches as `message.auth_json`. It runs lazily when a message is
//! opened.
//!
//! - **DKIM and ARC** are verified locally with `mail-auth`. A missing key on older
//!   mail is [`MechanismResult::KeyNoLongerPublished`], not a forgery.
//! - **DMARC** is derived from DKIM alignment. SPF can't be checked after delivery
//!   (it needs the connecting IP address), so it is
//!   [`MechanismResult::NotAvailable`] unless the provider reported it.
//! - **Provider results** come from the one `Authentication-Results` header that
//!   the account's receiving server wrote, recognized by position and
//!   `authserv-id` ([`provider`]): exact for Gmail and Microsoft 365, by the
//!   provider's domains for any other server. Everything else claiming to be a
//!   result is ignored.
//! - DNS goes through [`Resolver`]: hickory with a cache in production, a fixed
//!   [`StaticDns`] table in tests.
//!
//! # Phishing heuristics
//!
//! [`phishing`] has pure functions for link text mismatches, lookalike and
//! international domains, display-name spoofing and DMARC failures.
//!
//! # Privacy
//!
//! Addresses, domains and header content are message data. Types holding them
//! implement `Debug` with the content redacted, and this crate never logs them.

pub mod auth_results;
mod dns;
pub mod domain;
mod headers;
mod normalize;
pub mod phishing;
pub mod provider;
mod report;
mod verify;

pub use dns::{Resolver, ResolverError, StaticDns};
pub use normalize::{ProviderDkim, ProviderMechanism, ProviderResults};
pub use provider::{
    AuthservId, Boundary, HostMatcher, ProviderKind, ProviderOutcome, ProviderProfile,
    ProviderStatus, ResultsPlacement,
};
pub use report::{
    AUTH_REPORT_SCHEMA, AuthReport, Check, DkimSignatureCheck, DmarcPolicy, MechanismResult,
    Source, Verdict,
};
pub use verify::{Verifier, VerifyContext, VerifyOptions};

/// Re-exported so callers can configure [`Resolver::with_config`] without a direct
/// hickory dependency.
pub use mail_auth::hickory_resolver::config::{ResolverConfig, ResolverOpts};

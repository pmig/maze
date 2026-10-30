//! Error types and the mapping from OAuth error responses to them.

use std::fmt;
use std::io;

use mazemail_types::Sensitive;

use crate::{Capability, Provider, ProviderKind};

/// What the user sees when a Microsoft 365 organization blocks Maze until an
/// administrator approves it (CONCEPT §6). Shown for [`AuthError::AdminConsentRequired`].
pub const ADMIN_CONSENT_EXPLANATION: &str = "Your organization's Microsoft 365 settings \
don't allow Maze to access your mailbox yet. An administrator of your organization has to \
approve Maze first: in Microsoft Entra ID, under Enterprise applications, they grant admin \
consent for Maze (or assign you to it). Forward this message to your IT department and \
sign in again once they have approved it.";

/// Why the user has to go through the browser sign-in again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReauthReason {
    /// No refresh token is stored for the account: it was never signed in, was signed
    /// out, or the stored token was removed.
    NotSignedIn,
    /// The provider rejected the refresh token (`invalid_grant`): it expired, was
    /// revoked, or the account password changed.
    TokenRejected,
    /// The provider needs the user in the browser (`interaction_required`,
    /// `login_required`, `consent_required`), e.g. for a new MFA prompt.
    InteractionRequired,
    /// The account's grant doesn't cover `capability` (e.g. contacts on an account
    /// that signed in for mail only, or the user unticked it on the consent page).
    /// Only that capability is affected: mail keeps working. A sign-in with
    /// [`AuthorizeOptions::incremental_consent`](crate::AuthorizeOptions::incremental_consent)
    /// for the capability fixes it (the UI says "Contacts need permission").
    MissingScope {
        /// The capability without permission.
        capability: Capability,
    },
}

impl fmt::Display for ReauthReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSignedIn => f.write_str("the account is not signed in"),
            Self::TokenRejected => f.write_str("the provider rejected the saved sign-in"),
            Self::InteractionRequired => {
                f.write_str("the provider needs you to confirm the sign-in")
            }
            Self::MissingScope { capability } => {
                write!(
                    f,
                    "the account hasn't given Maze access to its {capability}"
                )
            }
        }
    }
}

/// An OAuth error response (RFC 6749 §4.1.2.1 and §5.2) from the authorization or
/// token endpoint.
///
/// The free-text description can name the user's account, so it is kept
/// [`Sensitive`] and left out of `Display`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthErrorDetails {
    /// The `error` code, e.g. `invalid_grant`.
    pub error: String,
    /// The `error_description`, if any.
    pub description: Option<Sensitive<String>>,
    /// Microsoft Entra `AADSTS` codes, from `error_codes` or the description.
    pub error_codes: Vec<u32>,
}

impl OAuthErrorDetails {
    /// Details from the raw response fields, extracting `AADSTS` codes.
    pub(crate) fn new(error: String, description: Option<String>, error_codes: Vec<u32>) -> Self {
        let mut codes = error_codes;
        if let Some(description) = &description {
            for code in aadsts_codes(description) {
                if !codes.contains(&code) {
                    codes.push(code);
                }
            }
        }
        Self {
            error,
            description: description.map(Sensitive::new),
            error_codes: codes,
        }
    }

    /// Whether any of `codes` is among the `AADSTS` codes.
    fn has_code(&self, codes: &[u32]) -> bool {
        self.error_codes.iter().any(|code| codes.contains(code))
    }
}

impl fmt::Display for OAuthErrorDetails {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)?;
        for code in &self.error_codes {
            write!(f, " AADSTS{code}")?;
        }
        Ok(())
    }
}

/// Everything that can go wrong while signing in or getting a token.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The user has to sign in again in the browser. The UI shows "Sign in again".
    #[error("sign in again: {reason}")]
    ReauthRequired {
        /// Why the saved sign-in can't be used.
        reason: ReauthReason,
    },

    /// A Microsoft 365 organization requires an administrator to approve Maze
    /// (`AADSTS65001`, `AADSTS90094`, `AADSTS90095`, `AADSTS50105`). Show
    /// [`ADMIN_CONSENT_EXPLANATION`].
    #[error("an administrator must approve Maze for this organization{}", fmt_code(*code))]
    AdminConsentRequired {
        /// The `AADSTS` code that triggered this, if known.
        code: Option<u32>,
    },

    /// The Microsoft app registration only allows accounts of its own organization
    /// (single-tenant), but Maze signed in through a multi-tenant endpoint such as
    /// `common` (`AADSTS50194`).
    #[error(
        "This app registration only allows its own organization. Set `auth.microsoft.tenant` \
         to your tenant ID or domain, or register the app for any organization."
    )]
    SingleTenantApp,

    /// Microsoft doesn't know the tenant in the endpoints (`AADSTS90002`,
    /// `AADSTS900023`, `AADSTS900021`): a typo in `auth.microsoft.tenant`, or a deleted
    /// directory.
    #[error("Microsoft doesn't know the tenant {tenant}")]
    UnknownTenant {
        /// The tenant Maze asked for.
        tenant: String,
    },

    /// The user declined consent or cancelled at the provider (`access_denied`,
    /// `AADSTS65004`).
    #[error("sign-in was cancelled or permission was declined ({0})")]
    AccessDenied(OAuthErrorDetails),

    /// The provider rejected Maze's client ID or secret (`invalid_client`,
    /// `unauthorized_client`). With a bring-your-own Google client ID, the user should
    /// check `auth.google.client_id` and `auth.google.client_secret`.
    #[error("{provider} rejected the configured OAuth client ({details})")]
    InvalidClient {
        /// Which provider.
        provider: ProviderKind,
        /// The error response.
        details: OAuthErrorDetails,
    },

    /// Any other OAuth error response.
    #[error("{provider} returned an OAuth error: {details}")]
    Provider {
        /// Which provider.
        provider: ProviderKind,
        /// The error response.
        details: OAuthErrorDetails,
    },

    /// The `state` in the redirect didn't match the request: a forged or stale redirect.
    #[error("the sign-in response did not match the request (state mismatch)")]
    StateMismatch,

    /// The browser sign-in didn't finish in time.
    #[error("the sign-in timed out")]
    Timeout,

    /// No client ID is configured for the provider (`auth.<provider>.client_id`).
    #[error("no OAuth client ID is configured for {provider} (set auth.{}.client_id)", provider.as_str())]
    MissingClientId {
        /// Which provider.
        provider: ProviderKind,
    },

    /// An endpoint URL isn't acceptable (not HTTPS and not loopback).
    #[error("invalid OAuth endpoint {url}: {reason}")]
    InvalidEndpoint {
        /// The rejected URL.
        url: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The token endpoint answered with an HTTP error and no OAuth error body
    /// (e.g. 429 or 5xx). Usually transient.
    #[error("the token endpoint answered HTTP {status}")]
    Http {
        /// The HTTP status code.
        status: u16,
    },

    /// The token endpoint couldn't be reached.
    #[error("network error talking to the token endpoint: {message}")]
    Network {
        /// The error, without URL.
        message: String,
        /// Whether it was a timeout.
        timeout: bool,
    },

    /// The provider sent something that isn't a valid OAuth response.
    #[error("invalid response from the provider: {0}")]
    InvalidResponse(String),

    /// The system browser couldn't be opened.
    #[error("could not open the browser: {0}")]
    Browser(#[source] io::Error),

    /// The loopback redirect listener failed.
    #[error("the local sign-in listener failed: {0}")]
    Loopback(#[source] io::Error),

    /// The system random number generator failed.
    #[error("the system random number generator failed: {0}")]
    Random(String),

    /// Reading or writing the secret store failed.
    #[error(transparent)]
    SecretStore(#[from] crate::SecretStoreError),
}

impl AuthError {
    /// Whether the UI should offer "Sign in again" (a browser sign-in can fix it).
    /// True for [`ReauthReason::MissingScope`] too; check
    /// [`missing_capability`](Self::missing_capability) first to offer the sign-in
    /// for that capability only.
    pub fn requires_reauth(&self) -> bool {
        matches!(self, Self::ReauthRequired { .. })
    }

    /// The capability the account's grant lacks ([`ReauthReason::MissingScope`]).
    pub fn missing_capability(&self) -> Option<Capability> {
        match self {
            Self::ReauthRequired {
                reason: ReauthReason::MissingScope { capability },
            } => Some(*capability),
            _ => None,
        }
    }

    /// [`AuthError::ReauthRequired`] with [`ReauthReason::MissingScope`].
    pub fn missing_scope(capability: Capability) -> Self {
        Self::ReauthRequired {
            reason: ReauthReason::MissingScope { capability },
        }
    }

    /// Whether retrying later without user action may succeed (network trouble,
    /// rate limits, server errors).
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network { .. } => true,
            Self::Http { status } => *status == 429 || *status >= 500,
            Self::Provider { details, .. } => {
                matches!(
                    details.error.as_str(),
                    "temporarily_unavailable" | "server_error"
                )
            }
            _ => false,
        }
    }

    pub(crate) fn network(err: reqwest::Error) -> Self {
        let timeout = err.is_timeout();
        // Without the URL: nothing secret is in it, but the message stays short.
        let err = err.without_url();
        let message = match std::error::Error::source(&err) {
            Some(source) => format!("{err}: {source}"),
            None => err.to_string(),
        };
        Self::Network { message, timeout }
    }
}

fn fmt_code(code: Option<u32>) -> String {
    code.map(|code| format!(" (AADSTS{code})"))
        .unwrap_or_default()
}

/// `AADSTS` codes for which an administrator has to act.
const ADMIN_CONSENT_CODES: &[u32] = &[
    // The user or administrator has not consented to use the application.
    65001, // An administrator has to consent (user consent is disabled for the tenant).
    90094, // Admin consent workflow: a request was sent to an administrator.
    90095, // The signed-in user is not assigned to a role for the application.
    50105,
];

/// `AADSTS` codes for "the user declined".
const DECLINED_CODES: &[u32] = &[65004];

/// `AADSTS50194`: a single-tenant app was used through a multi-tenant endpoint.
const SINGLE_TENANT_CODES: &[u32] = &[50194];

/// `AADSTS` codes for a tenant Microsoft doesn't know (not found, invalid name).
const UNKNOWN_TENANT_CODES: &[u32] = &[90002, 900023, 900021];

/// `AADSTS` codes for "the grant doesn't cover the requested scopes": not consented
/// (65001), and scopes unauthorized or expired (70000).
const MISSING_CONSENT_CODES: &[u32] = &[65001, 70000];

/// Maps an error response to a token request for a secondary resource's token
/// (Microsoft Graph for `capability`): "not consented" means the grant lacks
/// `capability` ([`ReauthReason::MissingScope`]), not that the account has to sign in
/// again or an administrator has to act, so mail is left alone. Anything else as
/// [`classify`].
pub(crate) fn classify_for(
    provider: &Provider,
    details: OAuthErrorDetails,
    capability: Option<Capability>,
) -> AuthError {
    if let Some(capability) = capability
        && (details.has_code(MISSING_CONSENT_CODES)
            || matches!(
                details.error.as_str(),
                "consent_required" | "interaction_required" | "invalid_scope"
            ))
    {
        return AuthError::missing_scope(capability);
    }
    classify(provider, details)
}

/// Maps an OAuth error response from `provider` to an [`AuthError`].
pub(crate) fn classify(provider: &Provider, details: OAuthErrorDetails) -> AuthError {
    if details.has_code(SINGLE_TENANT_CODES) {
        return AuthError::SingleTenantApp;
    }
    if details.has_code(UNKNOWN_TENANT_CODES) {
        let tenant = provider
            .tenant()
            .map_or_else(|| "in the sign-in address".to_owned(), ToString::to_string);
        return AuthError::UnknownTenant { tenant };
    }
    let provider = provider.kind();
    if details.has_code(ADMIN_CONSENT_CODES) {
        let code = details
            .error_codes
            .iter()
            .copied()
            .find(|code| ADMIN_CONSENT_CODES.contains(code));
        return AuthError::AdminConsentRequired { code };
    }
    if details.has_code(DECLINED_CODES) {
        return AuthError::AccessDenied(details);
    }
    match details.error.as_str() {
        "access_denied" => AuthError::AccessDenied(details),
        "invalid_grant" => AuthError::ReauthRequired {
            reason: ReauthReason::TokenRejected,
        },
        "interaction_required" | "login_required" | "consent_required" => {
            AuthError::ReauthRequired {
                reason: ReauthReason::InteractionRequired,
            }
        }
        "invalid_client" | "unauthorized_client" => AuthError::InvalidClient { provider, details },
        _ => AuthError::Provider { provider, details },
    }
}

/// `AADSTS<digits>` codes mentioned in a Microsoft error description.
fn aadsts_codes(text: &str) -> Vec<u32> {
    let mut codes = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("AADSTS") {
        rest = &rest[start + "AADSTS".len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if let Ok(code) = rest[..digits].parse::<u32>()
            && !codes.contains(&code)
        {
            codes.push(code);
        }
        rest = &rest[digits..];
    }
    codes
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn classify(kind: ProviderKind, details: OAuthErrorDetails) -> AuthError {
        super::classify(&Provider::builtin(kind), details)
    }

    fn details(error: &str, description: Option<&str>, codes: &[u32]) -> OAuthErrorDetails {
        OAuthErrorDetails::new(
            error.to_owned(),
            description.map(str::to_owned),
            codes.to_vec(),
        )
    }

    #[test]
    fn extracts_aadsts_codes_from_the_description() {
        assert_eq!(
            aadsts_codes("AADSTS65001: The user or administrator has not consented. AADSTS1."),
            vec![65001, 1]
        );
        assert_eq!(aadsts_codes("AADSTS: nothing"), Vec::<u32>::new());
        assert_eq!(aadsts_codes("no codes"), Vec::<u32>::new());
    }

    #[test]
    fn merges_error_codes_and_description_codes() {
        let d = details("invalid_grant", Some("AADSTS65001: x"), &[65001, 7000215]);
        assert_eq!(d.error_codes, vec![65001, 7000215]);
    }

    #[test]
    fn admin_consent_codes_win_over_invalid_grant() {
        for code in [65001, 90094, 90095, 50105] {
            let err = classify(
                ProviderKind::Microsoft,
                details(
                    "invalid_grant",
                    Some(&format!("AADSTS{code}: needs admin")),
                    &[],
                ),
            );
            assert!(
                matches!(err, AuthError::AdminConsentRequired { code: Some(c) } if c == code),
                "{err:?}"
            );
        }
    }

    #[test]
    fn maps_standard_errors() {
        let ms = ProviderKind::Microsoft;
        assert!(matches!(
            classify(ms, details("invalid_grant", None, &[70008])),
            AuthError::ReauthRequired {
                reason: ReauthReason::TokenRejected
            }
        ));
        assert!(matches!(
            classify(ms, details("interaction_required", None, &[50076])),
            AuthError::ReauthRequired {
                reason: ReauthReason::InteractionRequired
            }
        ));
        assert!(matches!(
            classify(ms, details("access_denied", None, &[])),
            AuthError::AccessDenied(_)
        ));
        assert!(matches!(
            classify(
                ms,
                details("invalid_request", Some("AADSTS65004: declined"), &[])
            ),
            AuthError::AccessDenied(_)
        ));
        assert!(matches!(
            classify(
                ProviderKind::Google,
                details("invalid_client", Some("Unauthorized"), &[])
            ),
            AuthError::InvalidClient {
                provider: ProviderKind::Google,
                ..
            }
        ));
        assert!(matches!(
            classify(ms, details("server_error", None, &[])),
            AuthError::Provider { .. }
        ));
    }

    #[test]
    fn tenant_errors_say_what_to_change() {
        let err = classify(
            ProviderKind::Microsoft,
            details(
                "invalid_request",
                Some(
                    "AADSTS50194: Application 'x'(Maze) is not configured as a multi-tenant application.",
                ),
                &[],
            ),
        );
        assert!(matches!(err, AuthError::SingleTenantApp), "{err:?}");
        assert_eq!(
            err.to_string(),
            "This app registration only allows its own organization. Set \
             `auth.microsoft.tenant` to your tenant ID or domain, or register the app for \
             any organization."
        );
        assert!(!err.requires_reauth() && !err.is_transient());

        let contoso = mazemail_types::MicrosoftTenant::parse("contso.com").unwrap();
        for code in [90002, 900023] {
            let err = super::classify(
                &Provider::microsoft(&contoso),
                details("invalid_request", None, &[code]),
            );
            assert_eq!(
                err.to_string(),
                "Microsoft doesn't know the tenant contso.com",
                "AADSTS{code}"
            );
        }
    }

    #[test]
    fn display_never_contains_the_description() {
        let err = classify(
            ProviderKind::Microsoft,
            details(
                "invalid_request",
                Some("AADSTS50020: User account 'bob@example.org' does not exist"),
                &[],
            ),
        );
        let text = format!("{err} {err:?}");
        assert!(!text.contains("bob@example.org"), "{text}");
        assert!(text.contains("AADSTS50020"), "{text}");
    }

    #[test]
    fn missing_consent_for_a_secondary_resource_is_a_missing_scope() {
        let ms = Provider::builtin(ProviderKind::Microsoft);
        for d in [
            details("invalid_grant", Some("AADSTS65001: not consented"), &[]),
            details("invalid_grant", None, &[70000]),
            details("consent_required", None, &[]),
            details("interaction_required", None, &[]),
            details("invalid_scope", None, &[]),
        ] {
            let err = classify_for(&ms, d.clone(), Some(Capability::Contacts));
            assert_eq!(
                err.missing_capability(),
                Some(Capability::Contacts),
                "{d}: {err:?}"
            );
            assert!(err.requires_reauth());
            assert_eq!(
                err.to_string(),
                "sign in again: the account hasn't given Maze access to its contacts"
            );
            // The primary token's errors stay what they were.
            assert_eq!(classify_for(&ms, d, None).missing_capability(), None);
        }
        // A dead refresh token stays a dead refresh token.
        assert!(matches!(
            classify_for(
                &ms,
                details("invalid_grant", None, &[70008]),
                Some(Capability::Contacts)
            ),
            AuthError::ReauthRequired {
                reason: ReauthReason::TokenRejected
            }
        ));
    }

    #[test]
    fn transient_errors() {
        assert!(AuthError::Http { status: 503 }.is_transient());
        assert!(AuthError::Http { status: 429 }.is_transient());
        assert!(!AuthError::Http { status: 404 }.is_transient());
        assert!(
            !AuthError::ReauthRequired {
                reason: ReauthReason::TokenRejected
            }
            .is_transient()
        );
        assert!(
            AuthError::ReauthRequired {
                reason: ReauthReason::NotSignedIn
            }
            .requires_reauth()
        );
    }
}

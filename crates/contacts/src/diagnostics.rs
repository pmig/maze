//! What a failed request to a contacts server leaves in the log: enough to tell what
//! went wrong and to ask the provider about it (the status, the server's error code,
//! its request id), never contact data. The route names the kind of call without ids
//! or mailbox addresses; the server's message loses the quoted values it may echo back
//! (an address, a phone number, a name).

use reqwest::Method;
use url::Url;

/// Path segments that name parts of the APIs and stay in [`route`]; every other
/// segment (contact and folder ids, mailbox addresses) becomes `{id}`.
const API_WORDS: &[&str] = &[
    "v1",
    "v1.0",
    "beta",
    "me",
    "users",
    "contacts",
    "contactFolders",
    "delta",
    "photo",
    "$value",
    "people",
    "connections",
    "otherContacts",
    "contactGroups",
];

/// How many characters of a server's message go into the log.
const MESSAGE_MAX_CHARS: usize = 300;

/// The kind of call `url` is, without whose: `v1.0/me/contacts/{id}`,
/// `v1/people/{id}:updateContact`.
pub(crate) fn route(url: &Url) -> String {
    let Some(segments) = url.path_segments() else {
        return String::new();
    };
    segments
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            // The People API puts a method after the resource: `people/c1:updateContact`.
            let (resource, method) = match segment.split_once(':') {
                Some((resource, method)) => (resource, Some(method)),
                None => (segment, None),
            };
            let resource = if API_WORDS.contains(&resource) {
                resource
            } else {
                "{id}"
            };
            match method {
                Some(method) if method.chars().all(|c| c.is_ascii_alphanumeric()) => {
                    format!("{resource}:{method}")
                }
                Some(_) => format!("{resource}:{{id}}"),
                None => resource.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// A server's error message for the log: quoted text with an `@`, a digit or a space
/// becomes `'…'` (it may be what the user entered), quoted names of properties stay,
/// unquoted words with an `@` become `<address>`. The text is cut after
/// [`MESSAGE_MAX_CHARS`] characters.
pub(crate) fn scrub(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // A quote opens at the start or after a space or `(`, `[`, `:`, `=`, `,`, so
        // the apostrophe in "contact's" doesn't. It also has to close.
        let opens = matches!(c, '\'' | '"')
            && (i == 0 || matches!(chars[i - 1], ' ' | '(' | '[' | ':' | '=' | ','));
        if opens && let Some(len) = chars[i + 1..].iter().position(|&next| next == c) {
            let quoted: String = chars[i + 1..i + 1 + len].iter().collect();
            let personal = quoted
                .chars()
                .any(|q| q == '@' || q.is_ascii_digit() || q.is_whitespace());
            out.push(c);
            out.push_str(if personal { "…" } else { &quoted });
            out.push(c);
            i += len + 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    let words: Vec<String> = out
        .split(' ')
        .map(|word| {
            if word.contains('@') && !word.starts_with(['\'', '"']) {
                "<address>".to_owned()
            } else {
                word.to_owned()
            }
        })
        .collect();
    let joined = words.join(" ");
    match joined.char_indices().nth(MESSAGE_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &joined[..cut]),
        None => joined,
    }
}

/// A request a contacts server answered with an error, as it goes into the log.
pub(crate) struct Failed<'a> {
    /// The API, as people know it ("Microsoft Graph", "Google People API").
    pub(crate) api: &'static str,
    pub(crate) method: &'a Method,
    pub(crate) url: &'a Url,
    pub(crate) status: u16,
    /// The server's error code (Graph's `code`, the People API's `status`).
    pub(crate) code: &'a str,
    /// The server's explanation, scrubbed before it's logged.
    pub(crate) message: &'a str,
    /// The server's id for the request (Graph's `request-id`), to ask the provider about
    /// it.
    pub(crate) request_id: Option<&'a str>,
    /// The id Maze sent with the request (Graph's `client-request-id`).
    pub(crate) client_request_id: Option<&'a str>,
}

impl Failed<'_> {
    /// Logs the failure: as a warning, except the answers the callers handle as an
    /// outcome (missing, changed elsewhere, an expired sync token), which are debug.
    pub(crate) fn log(&self) {
        let route = route(self.url);
        let message = scrub(self.message);
        let request_id = self.request_id.unwrap_or("");
        let client_request_id = self.client_request_id.unwrap_or("");
        if matches!(self.status, 404 | 409 | 410 | 412) {
            tracing::debug!(
                api = self.api,
                method = %self.method,
                route,
                status = self.status,
                code = self.code,
                message,
                request_id,
                client_request_id,
                "contacts request answered with an error"
            );
        } else {
            tracing::warn!(
                api = self.api,
                method = %self.method,
                route,
                status = self.status,
                code = self.code,
                message,
                request_id,
                client_request_id,
                "contacts request failed"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn url(path: &str) -> Url {
        Url::parse(&format!("https://api.example{path}")).unwrap()
    }

    #[test]
    fn routes_keep_the_api_and_drop_ids() {
        assert_eq!(route(&url("/v1.0/me/contacts")), "v1.0/me/contacts");
        assert_eq!(
            route(&url("/v1.0/me/contacts/AAMkAGI2=/photo/$value")),
            "v1.0/me/contacts/{id}/photo/$value"
        );
        assert_eq!(
            route(&url(
                "/v1.0/users/anna@example.com/contactFolders/F1/contacts/delta"
            )),
            "v1.0/users/{id}/contactFolders/{id}/contacts/delta"
        );
        assert_eq!(
            route(&url("/v1/people:createContact")),
            "v1/people:createContact"
        );
        assert_eq!(
            route(&url("/v1/people/c123:updateContact")),
            "v1/people/{id}:updateContact"
        );
    }

    #[test]
    fn messages_lose_what_the_user_entered() {
        assert_eq!(
            scrub("An internal server error occurred. The operation failed."),
            "An internal server error occurred. The operation failed."
        );
        assert_eq!(
            scrub("Property 'emailAddresses' has invalid value 'anna@example.com'."),
            "Property 'emailAddresses' has invalid value '…'."
        );
        assert_eq!(
            scrub("Invalid phone number \"+43 660 1\" for anna@example.com"),
            "Invalid phone number \"…\" for <address>"
        );
        assert_eq!(
            scrub("Name 'Anna Example' is too long"),
            "Name '…' is too long"
        );
        assert_eq!(
            scrub("The contact's folder doesn't exist"),
            "The contact's folder doesn't exist"
        );
        let long = "x".repeat(400);
        assert_eq!(scrub(&long).chars().count(), MESSAGE_MAX_CHARS + 1);
    }
}

//! The sender's mail client ("user agent"), from the headers it leaves behind.
//!
//! The approach follows Thunderbird's *Display Mail User Agent T* add-on; the tables
//! are Maze's own. Detection goes in three steps, first hit wins:
//!
//! 1. [`NAMED`]: the headers clients name themselves in ([`AGENT_HEADERS`]: first
//!    `User-Agent`, then `X-Mailer`, …), matched case-insensitively against an
//!    ordered list of substrings, more specific entries first (`neomutt` before
//!    `mutt`). Where a version number follows the substring it is kept, shortened to
//!    `major.minor`; Gecko-based clients also get the platform from their
//!    `Mozilla/5.0 (…)` comment.
//! 2. [`FINGERPRINTS`]: clients and bulk senders that don't name themselves, by
//!    headers only they add or their `Message-ID` domain (`…@mail.gmail.com`).
//! 3. A client-naming header that matched nothing is shown as sent, shortened.
//!
//! Every header here is set by the sender and can say anything: the result is a hint
//! for the reader, never a security signal.

use std::fmt;

use mail_parser::HeaderValue;
use mail_parser::parsers::MessageStream;
use serde::{Deserialize, Serialize};

/// The headers clients name themselves in, most authoritative first.
pub const AGENT_HEADERS: &[&str] = &["User-Agent", "X-Mailer", "X-Mail-Agent", "X-Newsreader"];

/// Longest [`MailClient::name`] built from an unrecognized header, in characters.
const MAX_UNKNOWN_NAME: usize = 40;

/// Longest [`MailClient::raw`], in characters.
const MAX_RAW: usize = 200;

/// The mail client a message was written with.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailClient {
    /// Normalized name: `Thunderbird 128.3 (Linux)`, `Apple Mail (iPhone)`, `Gmail`.
    pub name: String,
    /// The header it was recognized by, as spelled in the message (`X-Mailer`).
    pub header: String,
    /// That header's value, decoded and unfolded.
    pub raw: String,
}

impl fmt::Debug for MailClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailClient")
            .field("name", &self.name)
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

/// A client that names itself in one of the [`AGENT_HEADERS`].
#[derive(Debug, Clone, Copy)]
struct Named {
    /// Lowercase substring of the header value.
    token: &'static str,
    name: &'static str,
    /// Whether a version number follows `token`.
    version: bool,
    /// Whether to add the platform from a `Mozilla/5.0 (…)` comment.
    platform: bool,
}

const fn named(token: &'static str, name: &'static str) -> Named {
    Named {
        token,
        name,
        version: false,
        platform: false,
    }
}

const fn versioned(token: &'static str, name: &'static str) -> Named {
    Named {
        token,
        name,
        version: true,
        platform: false,
    }
}

const fn gecko(token: &'static str, name: &'static str) -> Named {
    Named {
        token,
        name,
        version: true,
        platform: true,
    }
}

/// Clients that name themselves, in match order.
const NAMED: &[Named] = &[
    // Mozilla family: forks before Thunderbird, whose token they may contain.
    gecko("betterbird", "Betterbird"),
    named("thunderbird for android", "Thunderbird for Android"),
    gecko("postboxapp", "Postbox"),
    gecko("postbox", "Postbox"),
    gecko("seamonkey", "SeaMonkey"),
    gecko("interlink", "Interlink"),
    gecko("icedove", "Icedove"),
    gecko("thunderbird", "Thunderbird"),
    // Microsoft.
    versioned("microsoft-macoutlook", "Outlook for Mac"),
    named("outlook-ios", "Outlook (iOS)"),
    named("outlook-android", "Outlook (Android)"),
    versioned("outlook express", "Outlook Express"),
    versioned("microsoft office outlook", "Outlook"),
    versioned("microsoft outlook", "Outlook"),
    versioned("windows live mail", "Windows Live Mail"),
    versioned("windows mail", "Windows Mail"),
    // Apple.
    named("iphone mail", "Apple Mail (iPhone)"),
    named("ipad mail", "Apple Mail (iPad)"),
    named("apple mail", "Apple Mail"),
    named("icloud", "iCloud Mail"),
    // Desktop and mobile apps.
    versioned("em_client", "eM Client"),
    versioned("em client", "eM Client"),
    versioned("mailmate", "MailMate"),
    named("airmail", "Airmail"),
    versioned("mailspring", "Mailspring"),
    named("canary mail", "Canary Mail"),
    named("superhuman", "Superhuman"),
    named("k-9 mail", "K-9 Mail"),
    named("fairemail", "FairEmail"),
    versioned("claws mail", "Claws Mail"),
    versioned("sylpheed", "Sylpheed"),
    versioned("evolution", "Evolution"),
    versioned("kmail", "KMail"),
    versioned("geary", "Geary"),
    versioned("balsa", "Balsa"),
    versioned("the bat!", "The Bat!"),
    versioned("pegasus mail", "Pegasus Mail"),
    versioned("foxmail", "Foxmail"),
    versioned("lotus notes release", "Lotus Notes"),
    versioned("lotus notes", "Lotus Notes"),
    // Terminal and Emacs: `neomutt` before `mutt`, `alpine` before `pine`.
    versioned("neomutt", "NeoMutt"),
    versioned("mutt", "Mutt"),
    versioned("alpine", "Alpine"),
    versioned("pine", "Pine"),
    versioned("aerc", "aerc"),
    versioned("himalaya", "Himalaya"),
    versioned("mu4e", "mu4e"),
    versioned("notmuch", "Notmuch"),
    versioned("gnus", "Gnus"),
    // Webmail.
    versioned("roundcube webmail", "Roundcube"),
    named("roundcube", "Roundcube"),
    versioned("squirrelmail", "SquirrelMail"),
    named("horde", "Horde"),
    versioned("snappymail", "SnappyMail"),
    versioned("rainloop", "RainLoop"),
    versioned("zimbra", "Zimbra"),
    named("open-xchange", "Open-Xchange"),
    named("sogo", "SOGo"),
    named("cyrus-jmap", "Fastmail"),
    named("ymail", "Yahoo Mail"),
    named("yahoomail", "Yahoo Mail"),
    // Bulk senders and libraries.
    named("mailchimp", "Mailchimp"),
    named("sparkpost", "SparkPost"),
    named("sendinblue", "Brevo"),
    versioned("phpmailer", "PHPMailer"),
    versioned("nodemailer", "Nodemailer"),
    versioned("swiftmailer", "Swift Mailer"),
    // Last: plain Emacs message mode, after the Emacs clients above.
    versioned("emacs", "Emacs"),
];

/// What a [`Fingerprint`] looks for. Header names are lowercase.
#[derive(Debug, Clone, Copy)]
enum Clue {
    /// The header is present.
    Has(&'static str),
    /// The header's value contains this (lowercase).
    Contains(&'static str, &'static str),
    /// Both headers are present with the same value.
    Same(&'static str, &'static str),
}

/// A client or bulk sender that doesn't name itself.
#[derive(Debug, Clone, Copy)]
struct Fingerprint {
    clue: Clue,
    name: &'static str,
}

const fn fingerprint(clue: Clue, name: &'static str) -> Fingerprint {
    Fingerprint { clue, name }
}

/// Fingerprints, in match order: services with headers of their own first, then
/// `Message-ID` domains, then the generic Exchange traces.
const FINGERPRINTS: &[Fingerprint] = &[
    // Notifications.
    fingerprint(Clue::Has("x-bugzilla-reason"), "Bugzilla"),
    fingerprint(Clue::Has("x-gitlab-project"), "GitLab"),
    fingerprint(Clue::Has("x-github-reason"), "GitHub"),
    // Bulk senders.
    fingerprint(Clue::Has("x-mc-user"), "Mailchimp"),
    fingerprint(Clue::Has("x-mandrill-user"), "Mailchimp Transactional"),
    fingerprint(Clue::Has("x-sg-eid"), "SendGrid"),
    fingerprint(Clue::Has("x-ses-outgoing"), "Amazon SES"),
    fingerprint(
        Clue::Contains("message-id", ".amazonses.com>"),
        "Amazon SES",
    ),
    fingerprint(Clue::Has("x-mailgun-sid"), "Mailgun"),
    fingerprint(Clue::Has("x-pm-message-id"), "Postmark"),
    fingerprint(Clue::Has("x-mj-mid"), "Mailjet"),
    fingerprint(
        Clue::Contains("message-id", "@smtp-relay.mailin.fr>"),
        "Brevo",
    ),
    // Clients without a User-Agent.
    fingerprint(Clue::Same("x-correlation-id", "message-id"), "FairEmail"),
    fingerprint(Clue::Contains("message-id", "@mail.gmail.com>"), "Gmail"),
    fingerprint(Clue::Contains("message-id", "@proton.me>"), "Proton Mail"),
    fingerprint(
        Clue::Contains("message-id", "@protonmail.com>"),
        "Proton Mail",
    ),
    fingerprint(
        Clue::Contains("message-id", "@app.fastmail.com>"),
        "Fastmail",
    ),
    fingerprint(
        Clue::Contains("message-id", "@mail.yahoo.com>"),
        "Yahoo Mail",
    ),
    fingerprint(Clue::Contains("message-id", "@spark>"), "Spark"),
    fingerprint(
        Clue::Contains("message-id", ".prod.outlook.com>"),
        "Outlook",
    ),
    fingerprint(Clue::Has("thread-index"), "Outlook"),
    fingerprint(
        Clue::Contains("x-ms-exchange-crosstenant-fromentityheader", "hosted"),
        "Microsoft 365",
    ),
    fingerprint(
        Clue::Contains("x-mimeole", "exchange"),
        "Microsoft Exchange",
    ),
    fingerprint(Clue::Contains("x-mimeole", "mimeole"), "Outlook"),
];

/// One header of the message, first occurrence of its name.
struct Field<'a> {
    /// Lowercase name.
    key: String,
    /// The name as spelled in the message.
    name: &'a str,
    /// Decoded and unfolded value.
    value: String,
    /// `value` in lowercase (same byte offsets: only ASCII is folded).
    lower: String,
}

/// Detects the mail client from a message's raw headers (name, value), in message
/// order. Values may be folded and RFC 2047 encoded. `None` if nothing gives the
/// client away.
pub fn detect_client<'a>(
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Option<MailClient> {
    let mut fields: Vec<Field<'a>> = Vec::new();
    for (name, value) in headers {
        let key = name.trim().to_ascii_lowercase();
        if !is_relevant(&key) || fields.iter().any(|f| f.key == key) {
            continue;
        }
        let value = decode(value);
        if value.is_empty() {
            continue;
        }
        let lower = value.to_ascii_lowercase();
        fields.push(Field {
            key,
            name: name.trim(),
            value,
            lower,
        });
    }
    let field = |key: &str| fields.iter().find(|f| f.key == key);
    let agents: Vec<&Field<'_>> = AGENT_HEADERS
        .iter()
        .filter_map(|h| fields.iter().find(|f| f.key.eq_ignore_ascii_case(h)))
        .collect();

    for agent in &agents {
        if let Some(name) = NAMED.iter().find_map(|rule| rule.recognize(&agent.lower)) {
            return Some(client(name, agent));
        }
    }
    for print in FINGERPRINTS {
        let hit = match print.clue {
            Clue::Has(key) => field(key),
            Clue::Contains(key, needle) => field(key).filter(|f| f.lower.contains(needle)),
            Clue::Same(key, other) => field(key)
                .zip(field(other))
                .filter(|(a, b)| a.value == b.value)
                .map(|(_, b)| b),
        };
        if let Some(hit) = hit {
            return Some(client(print.name.to_owned(), hit));
        }
    }
    agents
        .first()
        .and_then(|agent| Some(client(unknown_name(&agent.value)?, agent)))
}

/// Whether any rule looks at the header `key` (lowercase). Everything else is skipped
/// up front, which also bounds the work on messages with thousands of headers.
fn is_relevant(key: &str) -> bool {
    AGENT_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(key))
        || FINGERPRINTS.iter().any(|print| match print.clue {
            Clue::Has(k) | Clue::Contains(k, _) => k == key,
            Clue::Same(a, b) => a == key || b == key,
        })
}

impl Named {
    /// The display name if `lower` (a lowercase header value) matches.
    fn recognize(&self, lower: &str) -> Option<String> {
        let at = lower.find(self.token)?;
        let mut name = self.name.to_owned();
        if self.version
            && let Some(version) = version_after(&lower[at + self.token.len()..])
        {
            name.push(' ');
            name.push_str(version);
        }
        if self.platform
            && let Some(platform) = gecko_platform(lower)
        {
            name.push_str(" (");
            name.push_str(platform);
            name.push(')');
        }
        Some(name)
    }
}

fn client(name: String, field: &Field<'_>) -> MailClient {
    MailClient {
        name,
        header: field.name.to_owned(),
        raw: shorten(&field.value, MAX_RAW),
    }
}

/// The `major.minor` version at the start of `rest`, after up to two separators:
/// `/128.3.1` → `128.3`, ` 16.0` → `16.0`, ` (6.9.7;` → `6.9`.
fn version_after(rest: &str) -> Option<&str> {
    let start = rest
        .char_indices()
        .take(3)
        .find(|(_, c)| !matches!(c, '/' | ' ' | '(' | ':' | 'v'))
        .map(|(i, _)| i)?;
    let rest = &rest[start..];
    if !rest.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let len = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let version = rest[..len].trim_end_matches('.');
    let end = version
        .match_indices('.')
        .nth(1)
        .map_or(version.len(), |(i, _)| i);
    Some(&version[..end])
}

/// The platform in a Gecko user agent's comment (`Mozilla/5.0 (X11; Linux x86_64; …)`).
fn gecko_platform(lower: &str) -> Option<&'static str> {
    let comment = lower.split_once('(')?.1.split_once(')')?.0;
    [
        ("windows", "Windows"),
        ("macintosh", "macOS"),
        ("mac os x", "macOS"),
        ("android", "Android"),
        ("linux", "Linux"),
        ("freebsd", "FreeBSD"),
        ("openbsd", "OpenBSD"),
        ("x11", "Linux"),
    ]
    .into_iter()
    .find_map(|(token, platform)| comment.contains(token).then_some(platform))
}

/// A short name for an unrecognized client: the value up to its first comment or
/// URL, at most [`MAX_UNKNOWN_NAME`] characters.
fn unknown_name(value: &str) -> Option<String> {
    let cut = value
        .find(" (")
        .or_else(|| value.find(" <"))
        .unwrap_or(value.len());
    let name = value[..cut].trim().trim_end_matches([';', ',', '-']).trim();
    (!name.is_empty()).then(|| shorten(name, MAX_UNKNOWN_NAME))
}

/// `value`, cut to `max` characters with an ellipsis.
fn shorten(value: &str, max: usize) -> String {
    match value.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", value[..at].trim_end()),
        None => value.to_owned(),
    }
}

/// Unfolds a raw header value and decodes RFC 2047 encoded words.
fn decode(raw: &str) -> String {
    let decoded = if raw.contains("=?") {
        let line = format!("{}\n", raw.trim_start());
        match MessageStream::new(line.as_bytes()).parse_unstructured() {
            HeaderValue::Text(text) => text.into_owned(),
            _ => raw.to_owned(),
        }
    } else {
        raw.to_owned()
    };
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    /// The client name detected from `headers`.
    fn name(headers: &[(&str, &str)]) -> Option<String> {
        detect_client(headers.iter().copied()).map(|c| c.name)
    }

    /// The client name detected from a single header.
    fn one(header: &str, value: &str) -> Option<String> {
        name(&[(header, value)])
    }

    #[test]
    fn recognizes_real_world_agents() {
        let cases = [
            (
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Thunderbird/128.3.1",
                "Thunderbird 128.3 (Linux)",
            ),
            (
                "User-Agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:115.0) Gecko/20100101 Thunderbird/115.16.0",
                "Thunderbird 115.16 (macOS)",
            ),
            (
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Thunderbird/128.5.0",
                "Thunderbird 128.5 (Windows)",
            ),
            (
                "User-Agent",
                "Mozilla Thunderbird 1.0.2 (Windows/20050317)",
                "Thunderbird 1.0 (Windows)",
            ),
            (
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Betterbird/128.4.1esr-bb15",
                "Betterbird 128.4 (Linux)",
            ),
            (
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:102.0) Gecko/20100101 SeaMonkey/2.53.17",
                "SeaMonkey 2.53 (Linux)",
            ),
            (
                "User-Agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:68.0) Gecko/20100101 PostboxApp/7.0.60",
                "Postbox 7.0 (macOS)",
            ),
            ("X-Mailer", "Microsoft Outlook 16.0", "Outlook 16.0"),
            ("X-Mailer", "Microsoft Office Outlook 12.0", "Outlook 12.0"),
            (
                "X-Mailer",
                "Microsoft Outlook Express 6.00.2900.5512",
                "Outlook Express 6.00",
            ),
            (
                "User-Agent",
                "Microsoft-MacOutlook/16.89.24091630",
                "Outlook for Mac 16.89",
            ),
            (
                "X-Mailer",
                "Microsoft Windows Live Mail 16.4.3528.331",
                "Windows Live Mail 16.4",
            ),
            ("X-Mailer", "Apple Mail (2.3826.200.121)", "Apple Mail"),
            ("X-Mailer", "iPhone Mail (22B83)", "Apple Mail (iPhone)"),
            ("X-Mailer", "iPad Mail (21F90)", "Apple Mail (iPad)"),
            ("User-Agent", "eM_Client/10.1.4828.0", "eM Client 10.1"),
            ("X-Mailer", "Evolution 3.52.4-0ubuntu1", "Evolution 3.52"),
            (
                "X-Mailer",
                "Claws Mail 4.1.1 (GTK 3.24.38; x86_64-pc-linux-gnu)",
                "Claws Mail 4.1",
            ),
            ("User-Agent", "Mutt/2.2.12 (2023-09-09)", "Mutt 2.2"),
            ("User-Agent", "NeoMutt/20231103", "NeoMutt 20231103"),
            (
                "User-Agent",
                "Alpine 2.26 (DEB 649 2022-06-02)",
                "Alpine 2.26",
            ),
            ("User-Agent", "aerc/0.18.2", "aerc 0.18"),
            ("User-Agent", "mu4e 1.12.7; emacs 29.4", "mu4e 1.12"),
            (
                "User-Agent",
                "Notmuch/0.38.3 (https://notmuchmail.org) Emacs/29.4 (x86_64-pc-linux-gnu)",
                "Notmuch 0.38",
            ),
            ("User-Agent", "Gnus/5.13 (Gnus v5.13)", "Gnus 5.13"),
            ("User-Agent", "Roundcube Webmail/1.6.9", "Roundcube 1.6"),
            (
                "X-Mailer",
                "Zimbra 10.0.9_GA_4625 (ZimbraWebClient - FF131 (Mac)/10.0.9_GA_4625)",
                "Zimbra 10.0",
            ),
            (
                "User-Agent",
                "Cyrus-JMAP/3.11.0-alpha0-1108-g3a29173c6-fm-20241015",
                "Fastmail",
            ),
            ("X-Mailer", "WebService/1.1.22806 YMailNorrin", "Yahoo Mail"),
            ("User-Agent", "K-9 Mail for Android", "K-9 Mail"),
            (
                "X-Mailer",
                "PHPMailer 6.9.1 (https://github.com/PHPMailer/PHPMailer)",
                "PHPMailer 6.9",
            ),
            (
                "X-Mailer",
                "Nodemailer (6.9.15; +https://nodemailer.com/; SES API)",
                "Nodemailer 6.9",
            ),
            (
                "X-Mailer",
                "MailChimp Mailer - **CIDa1b2c3d4e5f6**",
                "Mailchimp",
            ),
            (
                "X-Newsreader",
                "Forte Agent 8.00/32.1186",
                "Forte Agent 8.00/32.1186",
            ),
        ];
        for (header, value, expected) in cases {
            assert_eq!(one(header, value).as_deref(), Some(expected), "{value}");
        }
    }

    #[test]
    fn fingerprints_clients_without_a_user_agent() {
        let cases: [(&[(&str, &str)], &str); 9] = [
            (
                &[("Message-ID", "<CAF=+kL3x9ZQ1+Yb7mQyH0a@mail.gmail.com>")],
                "Gmail",
            ),
            (
                &[(
                    "Message-ID",
                    "<AM0PR07MB4012ABCDEF0123456789@AM0PR07MB4012.eurprd07.prod.outlook.com>",
                )],
                "Outlook",
            ),
            (
                &[
                    ("Thread-Topic", "Budget"),
                    ("Thread-Index", "AdtYQ2l0aGVyZSBpcyBubyBzcG9vbg=="),
                    ("Message-ID", "<0123abcd@mail.example.com>"),
                ],
                "Outlook",
            ),
            (
                &[("Message-ID", "<k3Fz0Qxw9V8sY7aT_uR2q=@proton.me>")],
                "Proton Mail",
            ),
            (
                &[(
                    "Message-Id",
                    "<4c1e3b6a-6f0d-4c49-9b1e-2f8c0d1e2a3b@app.fastmail.com>",
                )],
                "Fastmail",
            ),
            (
                &[
                    ("X-SES-Outgoing", "2024.10.21-54.240.8.97"),
                    (
                        "Message-ID",
                        "<0100019270e5a1b2-aaaa-bbbb-000000@email.amazonses.com>",
                    ),
                ],
                "Amazon SES",
            ),
            (
                &[
                    ("X-SG-EID", "u001.AbCdEf"),
                    ("Message-ID", "<x@geopod-ismtpd-3>"),
                ],
                "SendGrid",
            ),
            (
                &[
                    ("Message-ID", "<a1b2c3@example.org>"),
                    ("X-Correlation-ID", "<a1b2c3@example.org>"),
                ],
                "FairEmail",
            ),
            (&[("X-GitHub-Reason", "mention")], "GitHub"),
        ];
        for (headers, expected) in cases {
            assert_eq!(name(headers).as_deref(), Some(expected), "{headers:?}");
        }
    }

    #[test]
    fn a_named_client_wins_over_fingerprints() {
        let headers = [
            ("Message-ID", "<CAF=abc@mail.gmail.com>"),
            ("Thread-Index", "AdtYQ2l0"),
            (
                "User-Agent",
                "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Thunderbird/128.3.1",
            ),
        ];
        assert_eq!(name(&headers).as_deref(), Some("Thunderbird 128.3 (Linux)"));
    }

    #[test]
    fn user_agent_wins_over_x_mailer() {
        let headers = [
            ("X-Mailer", "Microsoft Outlook 16.0"),
            ("User-Agent", "Mutt/2.2.12 (2023-09-09)"),
        ];
        assert_eq!(name(&headers).as_deref(), Some("Mutt 2.2"));
    }

    #[test]
    fn a_fingerprint_wins_over_an_unknown_agent() {
        let headers = [
            ("X-Mailer", "nlserver, Build 7.0.0.10651"),
            ("X-SES-Outgoing", "2024.10.21-54.240.8.97"),
        ];
        assert_eq!(name(&headers).as_deref(), Some("Amazon SES"));
    }

    #[test]
    fn unknown_agents_are_shortened() {
        assert_eq!(
            one(
                "X-Mailer",
                "Acme Bulk Sender 3.1 (build 42; +https://acme.example)"
            )
            .as_deref(),
            Some("Acme Bulk Sender 3.1")
        );
        let long = "x".repeat(100);
        let shortened = one("User-Agent", &long).unwrap_or_default();
        assert_eq!(shortened.chars().count(), MAX_UNKNOWN_NAME + 1);
        assert!(shortened.ends_with('…'));
    }

    #[test]
    fn keeps_the_header_and_raw_value() {
        let client = detect_client([("x-mailer", "  iPhone Mail\r\n (22B83)")]);
        assert_eq!(
            client,
            Some(MailClient {
                name: "Apple Mail (iPhone)".to_owned(),
                header: "x-mailer".to_owned(),
                raw: "iPhone Mail (22B83)".to_owned(),
            })
        );
    }

    #[test]
    fn decodes_encoded_words() {
        let client = detect_client([("X-Mailer", " =?UTF-8?B?VGhlIEJhdCEgMTAuNS4z?=")]);
        assert_eq!(
            client.map(|c| (c.name, c.raw)),
            Some(("The Bat! 10.5".to_owned(), "The Bat! 10.5.3".to_owned()))
        );
    }

    #[test]
    fn the_first_occurrence_of_a_header_counts() {
        let headers = [
            ("X-Mailer", "Apple Mail (2.3826)"),
            ("X-Mailer", "Evolution 3.52"),
        ];
        assert_eq!(name(&headers).as_deref(), Some("Apple Mail"));
    }

    #[test]
    fn nothing_to_go_on() {
        assert_eq!(name(&[]), None);
        assert_eq!(
            name(&[
                ("Message-ID", "<1234@example.org>"),
                ("User-Agent", "   "),
                ("Subject", "Hello"),
            ]),
            None
        );
    }

    #[test]
    fn versions() {
        assert_eq!(version_after("/128.3.1"), Some("128.3"));
        assert_eq!(version_after(" 16.0"), Some("16.0"));
        assert_eq!(version_after(" (6.9.15; x)"), Some("6.9"));
        assert_eq!(version_after(" v5.13)"), Some("5.13"));
        assert_eq!(version_after("/20231103"), Some("20231103"));
        assert_eq!(version_after("1."), Some("1"));
        assert_eq!(version_after(" for android"), None);
        assert_eq!(version_after(""), None);
    }

    #[test]
    fn never_panics_on_odd_input() {
        for value in [
            "(",
            ")",
            "thunderbird/",
            "thunderbird (",
            "ü thunderbird ü",
            "=?",
            "=?x?q?",
        ] {
            let _ = one("User-Agent", value);
        }
    }
}

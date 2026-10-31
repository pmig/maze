//! vCard 4.0 (RFC 6350) for [`ContactFields`]: what the `contact` table's `vcard`
//! column holds for synced contacts, so the Address Book shows a contact's details
//! without knowing the server's format.
//!
//! [`to_vcard`] writes `FN`, `N`, `EMAIL`, `TEL`, `ORG`, `TITLE`, `ADR`, `URL` and
//! `NOTE` (labels as `TYPE=`), escaped and folded at 75 octets with CRLF line ends;
//! [`parse`] reads those properties back (and ignores everything else).

use crate::model::{ContactFields, Labeled, Organization, PostalAddress, non_empty};

/// The vCard of `fields`; `uid` becomes the `UID` property (the server's id).
pub fn to_vcard(fields: &ContactFields, uid: Option<&str>) -> String {
    let mut out = String::new();
    line(&mut out, "BEGIN:VCARD");
    line(&mut out, "VERSION:4.0");
    if let Some(uid) = non_empty(uid) {
        property(&mut out, "UID", &[], &escape(uid));
    }
    // FN is required: the full name, else the first address, else empty.
    let full = fields
        .name
        .full()
        .or_else(|| fields.email_addresses().into_iter().next())
        .unwrap_or_default();
    property(&mut out, "FN", &[], &escape(&full));
    if fields.name.has_parts() {
        let name = &fields.name;
        property(
            &mut out,
            "N",
            &[],
            &compound(&[
                name.family.as_deref(),
                name.given.as_deref(),
                name.middle.as_deref(),
                name.prefix.as_deref(),
                name.suffix.as_deref(),
            ]),
        );
    }
    for email in &fields.emails {
        if let Some(value) = non_empty(Some(&email.value)) {
            property(&mut out, "EMAIL", &type_param(email), &escape(value));
        }
    }
    for phone in &fields.phones {
        if let Some(value) = non_empty(Some(&phone.value)) {
            property(&mut out, "TEL", &type_param(phone), &escape(value));
        }
    }
    for org in &fields.organizations {
        if non_empty(org.name.as_deref()).is_some()
            || non_empty(org.department.as_deref()).is_some()
        {
            property(
                &mut out,
                "ORG",
                &[],
                &compound(&[org.name.as_deref(), org.department.as_deref()]),
            );
        }
        if let Some(title) = non_empty(org.title.as_deref()) {
            property(&mut out, "TITLE", &[], &escape(title));
        }
    }
    for address in &fields.addresses {
        let mut params = Vec::new();
        if let Some(kind) = non_empty(address.kind.as_deref()) {
            params.push(format!("TYPE={}", param_value(kind)));
        }
        if let Some(formatted) = non_empty(address.formatted.as_deref()) {
            params.push(format!("LABEL=\"{}\"", label_value(formatted)));
        }
        property(
            &mut out,
            "ADR",
            &params,
            &compound(&[
                address.po_box.as_deref(),
                address.extended.as_deref(),
                address.street.as_deref(),
                address.city.as_deref(),
                address.region.as_deref(),
                address.postal_code.as_deref(),
                address.country.as_deref(),
            ]),
        );
    }
    for url in &fields.urls {
        if let Some(value) = non_empty(Some(&url.value)) {
            property(&mut out, "URL", &type_param(url), value);
        }
    }
    if let Some(note) = non_empty(fields.note.as_deref()) {
        property(&mut out, "NOTE", &[], &escape(note));
    }
    line(&mut out, "END:VCARD");
    out
}

/// The fields of a vCard written by [`to_vcard`] (or any vCard 3/4 with those
/// properties). Unknown properties are ignored; `None` if it isn't a vCard.
pub fn parse(text: &str) -> Option<ContactFields> {
    let mut lines = unfold(text).into_iter();
    if !lines.next()?.eq_ignore_ascii_case("BEGIN:VCARD") {
        return None;
    }
    let mut fields = ContactFields::default();
    let mut full = None;
    for raw in lines {
        let Some((head, value)) = split_value(&raw) else {
            continue;
        };
        let mut parts = split_params(head).into_iter();
        let name = parts.next().unwrap_or_default();
        // A group prefix (`item1.EMAIL`) is dropped.
        let name = name.rsplit('.').next().unwrap_or(name).to_ascii_uppercase();
        let params: Vec<(String, String)> = parts
            .filter_map(|p| {
                let (k, v) = p.split_once('=')?;
                Some((k.to_ascii_uppercase(), v.trim_matches('"').to_owned()))
            })
            .collect();
        let kind = params
            .iter()
            .find(|(k, _)| k == "TYPE")
            .map(|(_, v)| v.split(',').next().unwrap_or(v).to_ascii_lowercase())
            .filter(|v| v != "internet" && v != "pref" && !v.is_empty());
        match name.as_str() {
            "END" => break,
            "FN" => full = Some(unescape(value)),
            "N" => {
                let parts = split_compound(value);
                let part = |i: usize| parts.get(i).cloned().filter(|p| !p.is_empty());
                fields.name.family = part(0);
                fields.name.given = part(1);
                fields.name.middle = part(2);
                fields.name.prefix = part(3);
                fields.name.suffix = part(4);
            }
            "EMAIL" => fields.emails.push(Labeled {
                value: unescape(value),
                kind,
            }),
            "TEL" => fields.phones.push(Labeled {
                value: unescape(value),
                kind,
            }),
            "URL" => fields.urls.push(Labeled {
                value: value.to_owned(),
                kind,
            }),
            "ORG" => {
                let parts = split_compound(value);
                fields.organizations.push(Organization {
                    name: parts.first().cloned().filter(|p| !p.is_empty()),
                    department: parts.get(1).cloned().filter(|p| !p.is_empty()),
                    title: None,
                });
            }
            "TITLE" => {
                let title = Some(unescape(value)).filter(|t| !t.is_empty());
                match fields
                    .organizations
                    .iter_mut()
                    .find(|org| org.title.is_none())
                {
                    Some(org) => org.title = title,
                    None => fields.organizations.push(Organization {
                        title,
                        ..Organization::default()
                    }),
                }
            }
            "ADR" => {
                let parts = split_compound(value);
                let part = |i: usize| parts.get(i).cloned().filter(|p| !p.is_empty());
                let label = params
                    .iter()
                    .find(|(k, _)| k == "LABEL")
                    .map(|(_, v)| unescape(v))
                    .filter(|v| !v.is_empty());
                fields.addresses.push(PostalAddress {
                    formatted: label,
                    po_box: part(0),
                    extended: part(1),
                    street: part(2),
                    city: part(3),
                    region: part(4),
                    postal_code: part(5),
                    country: part(6),
                    kind,
                });
            }
            "NOTE" => fields.note = Some(unescape(value)).filter(|n| !n.is_empty()),
            _ => {}
        }
    }
    let full = full.filter(|f| !f.is_empty());
    // FN falls back to the address when there is no name; that isn't a name.
    let derived = fields.name.full();
    let address_only = derived.is_none() && fields.email_addresses().first() == full.as_ref();
    if full.is_some() && full != derived && !address_only {
        fields.name.display = full;
    }
    Some(fields)
}

fn line(out: &mut String, text: &str) {
    out.push_str(text);
    out.push_str("\r\n");
}

/// Writes `NAME;PARAMS:value`, folded at 75 octets.
fn property(out: &mut String, name: &str, params: &[String], value: &str) {
    let mut text = String::from(name);
    for param in params {
        text.push(';');
        text.push_str(param);
    }
    text.push(':');
    text.push_str(value);
    fold(out, &text);
}

/// RFC 6350 §3.2: lines longer than 75 octets continue on the next line after a
/// space, never inside a UTF-8 sequence.
fn fold(out: &mut String, text: &str) {
    let mut width = 0;
    for ch in text.chars() {
        let len = ch.len_utf8();
        if width + len > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(ch);
        width += len;
    }
    out.push_str("\r\n");
}

fn type_param(value: &Labeled) -> Vec<String> {
    non_empty(value.kind.as_deref())
        .map(|kind| vec![format!("TYPE={}", param_value(kind))])
        .unwrap_or_default()
}

/// A parameter value: characters that would end it are dropped or quoted.
fn param_value(value: &str) -> String {
    let clean: String = value
        .chars()
        .filter(|c| !matches!(c, '"' | '\r' | '\n'))
        .collect();
    if clean.contains([';', ':', ',']) {
        format!("\"{clean}\"")
    } else {
        clean
    }
}

/// A quoted `LABEL` value: newlines as `\n`, no double quotes.
fn label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace("\r\n", "\\n")
        .replace(['\n', '\r'], "\\n")
        .replace('"', "'")
}

/// RFC 6350 §3.4 text escaping.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\\n");
            }
            '\n' => out.push_str("\\n"),
            ch => out.push(ch),
        }
    }
    out
}

fn compound(parts: &[Option<&str>]) -> String {
    parts
        .iter()
        .map(|part| escape(part.map(str::trim).unwrap_or_default()))
        .collect::<Vec<_>>()
        .join(";")
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Splits a compound value at unescaped `;` and unescapes each part.
fn split_compound(value: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                current.push('\\');
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ';' => parts.push(unescape(&std::mem::take(&mut current))),
            ch => current.push(ch),
        }
    }
    parts.push(unescape(&current));
    parts
}

/// Splits `NAME;PARAM=…;PARAM=…` at `;` outside quoted parameter values.
fn split_params(head: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut start = 0;
    for (i, ch) in head.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(&head[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&head[start..]);
    parts
}

/// Splits `NAME;PARAMS:value` at the first `:` outside a quoted parameter value.
fn split_value(line: &str) -> Option<(&str, &str)> {
    let mut quoted = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            ':' if !quoted => return Some((&line[..i], &line[i + 1..])),
            _ => {}
        }
    }
    None
}

/// Joins folded lines (a line starting with a space or tab continues the previous).
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = raw.strip_prefix([' ', '\t'])
            && let Some(last) = lines.last_mut()
        {
            last.push_str(rest);
            continue;
        }
        if !raw.is_empty() {
            lines.push(raw.to_owned());
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::model::Name;

    fn sample() -> ContactFields {
        ContactFields {
            name: Name {
                display: None,
                given: Some("Anna".into()),
                family: Some("Example; Jr".into()),
                middle: None,
                prefix: Some("Dr.".into()),
                suffix: None,
            },
            emails: vec![
                Labeled::with_kind("anna@example.com", "home"),
                Labeled::new("anna@work.example"),
            ],
            phones: vec![Labeled::with_kind("+43 1 234", "mobile")],
            organizations: vec![Organization {
                name: Some("Glasskube, Inc.".into()),
                department: Some("R&D".into()),
                title: Some("CTO".into()),
            }],
            addresses: vec![PostalAddress {
                formatted: Some("Hauptstraße 1\n1010 Wien".into()),
                street: Some("Hauptstraße 1".into()),
                city: Some("Wien".into()),
                postal_code: Some("1010".into()),
                country: Some("Austria".into()),
                kind: Some("work".into()),
                ..PostalAddress::default()
            }],
            urls: vec![Labeled::new("https://example.com/a,b")],
            note: Some("Met at the conference.\nLikes tea; no sugar, really.".into()),
        }
    }

    #[test]
    fn writes_a_vcard_4() {
        let text = to_vcard(&sample(), Some("people/c1"));
        assert!(text.starts_with("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:people/c1\r\n"));
        assert!(text.contains("\r\nFN:Dr. Anna Example\\; Jr\r\n"), "{text}");
        assert!(
            text.contains("\r\nN:Example\\; Jr;Anna;;Dr.;\r\n"),
            "{text}"
        );
        assert!(text.contains("\r\nEMAIL;TYPE=home:anna@example.com\r\n"));
        assert!(text.contains("\r\nEMAIL:anna@work.example\r\n"));
        assert!(text.contains("\r\nTEL;TYPE=mobile:+43 1 234\r\n"));
        assert!(text.contains("\r\nORG:Glasskube\\, Inc.;R&D\r\n"));
        assert!(text.contains("\r\nTITLE:CTO\r\n"));
        assert!(text.ends_with("END:VCARD\r\n"));
        for line in text.split("\r\n") {
            assert!(line.len() <= 75, "{line:?} is longer than 75 octets");
        }
    }

    #[test]
    fn round_trips() {
        let fields = sample();
        let parsed = parse(&to_vcard(&fields, Some("people/c1"))).unwrap();
        assert_eq!(parsed, fields);
        // Long values are folded and unfolded, multi-byte characters intact.
        let long = ContactFields {
            name: Name {
                display: Some("Ünïcödé ".repeat(20).trim().to_owned()),
                ..Name::default()
            },
            ..ContactFields::default()
        };
        let text = to_vcard(&long, None);
        assert!(text.contains("\r\n "));
        assert_eq!(parse(&text).unwrap(), long);
    }

    #[test]
    fn an_address_only_contact_has_no_name() {
        let fields = ContactFields {
            emails: vec![Labeled::new("x@example.com")],
            ..ContactFields::default()
        };
        let text = to_vcard(&fields, None);
        assert!(text.contains("\r\nFN:x@example.com\r\n"));
        assert!(!text.contains("\r\nN:"));
        assert_eq!(parse(&text).unwrap(), fields);
        assert_eq!(parse("not a vcard"), None);
    }
}

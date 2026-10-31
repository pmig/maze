//! The People API's JSON (`Person`, list responses, errors) and its mapping to and
//! from [`Person`] / [`ContactFields`].

use serde::{Deserialize, Serialize};

use crate::model::{
    ContactFields, Labeled, Name, Organization, Person, PostalAddress, SourceRef, non_empty,
};

/// The size Maze asks Google to scale photos to (`=s96`).
pub(crate) const PHOTO_SIZE: u32 = 96;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GSource {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GFieldMetadata {
    pub(crate) primary: Option<bool>,
    pub(crate) source: Option<GSource>,
}

impl GFieldMetadata {
    fn is_primary(meta: Option<&Self>) -> bool {
        meta.and_then(|m| m.primary).unwrap_or(false)
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GPersonMetadata {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) sources: Vec<GSource>,
    #[serde(skip_serializing)]
    pub(crate) deleted: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GName {
    #[serde(skip_serializing)]
    pub(crate) metadata: Option<GFieldMetadata>,
    #[serde(skip_serializing)]
    pub(crate) display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) family_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) given_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) middle_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) honorific_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) honorific_suffix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) unstructured_name: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GValue {
    #[serde(skip_serializing)]
    pub(crate) metadata: Option<GFieldMetadata>,
    pub(crate) value: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GOrganization {
    #[serde(skip_serializing)]
    pub(crate) metadata: Option<GFieldMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) department: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GAddress {
    #[serde(skip_serializing)]
    pub(crate) metadata: Option<GFieldMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) formatted_value: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub(crate) kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) po_box: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) street_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) extended_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) region: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) postal_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) country: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GBiography {
    #[serde(skip_serializing)]
    pub(crate) metadata: Option<GFieldMetadata>,
    pub(crate) value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GPhoto {
    pub(crate) metadata: Option<GFieldMetadata>,
    pub(crate) url: Option<String>,
    pub(crate) default: Option<bool>,
}

/// The People API's `Person` (the fields Maze reads and writes).
#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GPerson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resource_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) metadata: Option<GPersonMetadata>,
    pub(crate) names: Vec<GName>,
    pub(crate) email_addresses: Vec<GValue>,
    pub(crate) phone_numbers: Vec<GValue>,
    pub(crate) organizations: Vec<GOrganization>,
    pub(crate) addresses: Vec<GAddress>,
    pub(crate) urls: Vec<GValue>,
    pub(crate) biographies: Vec<GBiography>,
    #[serde(skip_serializing)]
    pub(crate) photos: Vec<GPhoto>,
}

/// `people.connections.list`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GConnections {
    pub(crate) connections: Vec<GPerson>,
    pub(crate) next_page_token: Option<String>,
    pub(crate) next_sync_token: Option<String>,
}

/// `otherContacts.list`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct GOtherContacts {
    pub(crate) other_contacts: Vec<GPerson>,
    pub(crate) next_page_token: Option<String>,
    pub(crate) next_sync_token: Option<String>,
}

/// Google's error body (`{"error": {...}}`).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct GErrorBody {
    pub(crate) error: GError,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct GError {
    pub(crate) message: String,
    pub(crate) status: String,
    pub(crate) details: Vec<GErrorDetail>,
    pub(crate) errors: Vec<GErrorDetail>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct GErrorDetail {
    pub(crate) reason: Option<String>,
}

impl GError {
    pub(crate) fn parse(body: &[u8]) -> Self {
        serde_json::from_slice::<GErrorBody>(body)
            .map(|b| b.error)
            .unwrap_or_default()
    }

    pub(crate) fn has_reason(&self, reason: &str) -> bool {
        self.details.iter().chain(&self.errors).any(|d| {
            d.reason
                .as_deref()
                .is_some_and(|r| r.eq_ignore_ascii_case(reason))
        })
    }

    fn message_has(&self, needle: &str) -> bool {
        self.message.to_ascii_lowercase().contains(needle)
    }

    pub(crate) fn is_expired_sync_token(&self) -> bool {
        self.has_reason("EXPIRED_SYNC_TOKEN")
            || (self.message_has("sync token") && self.message_has("expired"))
    }

    pub(crate) fn is_missing_scope(&self) -> bool {
        self.has_reason("ACCESS_TOKEN_SCOPE_INSUFFICIENT")
            || self.has_reason("insufficientPermissions")
            || self.message_has("insufficient authentication scopes")
    }

    pub(crate) fn is_api_disabled(&self) -> bool {
        self.has_reason("SERVICE_DISABLED")
            || self.has_reason("accessNotConfigured")
            || self.message_has("has not been used in project")
    }

    pub(crate) fn is_rate_limit(&self) -> bool {
        self.has_reason("RATE_LIMIT_EXCEEDED")
            || self.has_reason("rateLimitExceeded")
            || self.has_reason("userRateLimitExceeded")
            || self.status == "RESOURCE_EXHAUSTED"
    }

    pub(crate) fn is_failed_precondition(&self) -> bool {
        self.status == "FAILED_PRECONDITION" || self.has_reason("failedPrecondition")
    }
}

fn text(value: Option<&str>) -> Option<String> {
    non_empty(value).map(str::to_owned)
}

fn kind(value: Option<&str>) -> Option<String> {
    non_empty(value).map(str::to_lowercase)
}

/// Values of a multi-valued field, the primary one first, without duplicates.
fn labeled(values: &[GValue], case_insensitive: bool) -> Vec<Labeled> {
    let mut sorted: Vec<&GValue> = values.iter().collect();
    sorted.sort_by_key(|v| !GFieldMetadata::is_primary(v.metadata.as_ref()));
    let mut out: Vec<Labeled> = Vec::new();
    for value in sorted {
        let Some(text) = text(value.value.as_deref()) else {
            continue;
        };
        let duplicate = out.iter().any(|o| {
            if case_insensitive {
                o.value.eq_ignore_ascii_case(&text)
            } else {
                o.value == text
            }
        });
        if !duplicate {
            out.push(Labeled {
                value: text,
                kind: kind(value.kind.as_deref()),
            });
        }
    }
    out
}

/// `url` scaled to [`PHOTO_SIZE`]: Google's size option (`=s100`) is replaced, or
/// added when there is none.
pub(crate) fn sized_photo_url(url: &str) -> String {
    let size = format!("s{PHOTO_SIZE}");
    let last_segment = url.rsplit('/').next().unwrap_or(url);
    if let Some(eq) = last_segment.rfind('=') {
        let options = &last_segment[eq + 1..];
        let start = url.len() - last_segment.len() + eq + 1;
        let replaced = options
            .split('-')
            .map(|option| {
                let is_size = option.len() > 1
                    && option.starts_with('s')
                    && option[1..].bytes().all(|b| b.is_ascii_digit());
                if is_size { size.as_str() } else { option }
            })
            .collect::<Vec<_>>()
            .join("-");
        let replaced = if replaced.split('-').any(|o| o == size) {
            replaced
        } else if replaced.is_empty() {
            size
        } else {
            format!("{size}-{replaced}")
        };
        return format!("{}{replaced}", &url[..start]);
    }
    format!("{url}={size}")
}

impl GPerson {
    /// The person in Maze's model.
    pub(crate) fn into_person(self) -> Person {
        let deleted = self
            .metadata
            .as_ref()
            .and_then(|m| m.deleted)
            .unwrap_or(false);
        let mut names: Vec<&GName> = self.names.iter().collect();
        names.sort_by_key(|n| !GFieldMetadata::is_primary(n.metadata.as_ref()));
        let name = names
            .first()
            .map(|n| Name {
                display: text(n.display_name.as_deref())
                    .or_else(|| text(n.unstructured_name.as_deref())),
                given: text(n.given_name.as_deref()),
                family: text(n.family_name.as_deref()),
                middle: text(n.middle_name.as_deref()),
                prefix: text(n.honorific_prefix.as_deref()),
                suffix: text(n.honorific_suffix.as_deref()),
            })
            .unwrap_or_default();
        let mut organizations: Vec<Organization> = Vec::new();
        for org in &self.organizations {
            let org = Organization {
                name: text(org.name.as_deref()),
                department: text(org.department.as_deref()),
                title: text(org.title.as_deref()),
            };
            if (org.name.is_some() || org.department.is_some() || org.title.is_some())
                && !organizations.contains(&org)
            {
                organizations.push(org);
            }
        }
        let mut addresses: Vec<PostalAddress> = Vec::new();
        for address in &self.addresses {
            let address = PostalAddress {
                formatted: text(address.formatted_value.as_deref()),
                po_box: text(address.po_box.as_deref()),
                extended: text(address.extended_address.as_deref()),
                street: text(address.street_address.as_deref()),
                city: text(address.city.as_deref()),
                region: text(address.region.as_deref()),
                postal_code: text(address.postal_code.as_deref()),
                country: text(address.country.as_deref()),
                kind: kind(address.kind.as_deref()),
            };
            if address != PostalAddress::default() && !addresses.contains(&address) {
                addresses.push(address);
            }
        }
        let note = self
            .biographies
            .iter()
            .find_map(|b| text(b.value.as_deref()));
        let mut photos: Vec<&GPhoto> = self.photos.iter().collect();
        photos.sort_by_key(|p| !GFieldMetadata::is_primary(p.metadata.as_ref()));
        // Google's generated letter avatars (`default: true`) aren't photos.
        let photo_url = photos
            .into_iter()
            .filter(|p| p.default != Some(true))
            .find_map(|p| text(p.url.as_deref()))
            .map(|url| sized_photo_url(&url));
        let sources = self
            .metadata
            .map(|m| {
                m.sources
                    .into_iter()
                    .map(|s| SourceRef {
                        kind: s.kind,
                        id: s.id,
                        etag: s.etag,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Person {
            resource_name: self.resource_name.unwrap_or_default(),
            etag: self.etag,
            fields: ContactFields {
                name,
                emails: labeled(&self.email_addresses, true),
                phones: labeled(&self.phone_numbers, false),
                organizations,
                addresses,
                urls: labeled(&self.urls, false),
                note,
            },
            photo_url,
            sources,
            deleted,
        }
    }

    /// The JSON to create or update a contact with `fields`.
    pub(crate) fn from_fields(fields: &ContactFields) -> Self {
        let name = &fields.name;
        let names = if name.has_parts() {
            vec![GName {
                given_name: text(name.given.as_deref()),
                family_name: text(name.family.as_deref()),
                middle_name: text(name.middle.as_deref()),
                honorific_prefix: text(name.prefix.as_deref()),
                honorific_suffix: text(name.suffix.as_deref()),
                ..GName::default()
            }]
        } else if let Some(display) = text(name.display.as_deref()) {
            vec![GName {
                unstructured_name: Some(display),
                ..GName::default()
            }]
        } else {
            Vec::new()
        };
        let values = |values: &[Labeled]| -> Vec<GValue> {
            values
                .iter()
                .filter_map(|v| {
                    Some(GValue {
                        metadata: None,
                        value: Some(text(Some(&v.value))?),
                        kind: text(v.kind.as_deref()),
                    })
                })
                .collect()
        };
        Self {
            names,
            email_addresses: values(&fields.emails),
            phone_numbers: values(&fields.phones),
            organizations: fields
                .organizations
                .iter()
                .map(|o| GOrganization {
                    metadata: None,
                    name: text(o.name.as_deref()),
                    department: text(o.department.as_deref()),
                    title: text(o.title.as_deref()),
                })
                .collect(),
            addresses: fields
                .addresses
                .iter()
                .map(|a| GAddress {
                    metadata: None,
                    formatted_value: text(a.formatted.as_deref()),
                    kind: text(a.kind.as_deref()),
                    po_box: text(a.po_box.as_deref()),
                    street_address: text(a.street.as_deref()),
                    extended_address: text(a.extended.as_deref()),
                    city: text(a.city.as_deref()),
                    region: text(a.region.as_deref()),
                    postal_code: text(a.postal_code.as_deref()),
                    country: text(a.country.as_deref()),
                })
                .collect(),
            urls: values(&fields.urls),
            biographies: text(fields.note.as_deref())
                .map(|note| GBiography {
                    metadata: None,
                    value: Some(note),
                    content_type: Some("TEXT_PLAIN".to_owned()),
                })
                .into_iter()
                .collect(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn photo_urls_are_sized() {
        assert_eq!(
            sized_photo_url("https://lh3.googleusercontent.com/a/abc=s100"),
            "https://lh3.googleusercontent.com/a/abc=s96"
        );
        assert_eq!(
            sized_photo_url("https://lh3.googleusercontent.com/a/abc=s100-p"),
            "https://lh3.googleusercontent.com/a/abc=s96-p"
        );
        assert_eq!(
            sized_photo_url("https://lh3.googleusercontent.com/a/abc=p"),
            "https://lh3.googleusercontent.com/a/abc=s96-p"
        );
        assert_eq!(
            sized_photo_url("https://lh3.googleusercontent.com/a/abc"),
            "https://lh3.googleusercontent.com/a/abc=s96"
        );
    }

    #[test]
    fn reads_a_person() {
        let json = serde_json::json!({
            "resourceName": "people/c1",
            "etag": "%EgUBAi43PRoEAQIFByIMR",
            "metadata": {"sources": [{"type": "CONTACT", "id": "abc", "etag": "#src"}]},
            "names": [
                {"metadata": {"source": {"type": "PROFILE"}}, "displayName": "Profile Name"},
                {"metadata": {"primary": true}, "displayName": "Anna Example",
                 "givenName": "Anna", "familyName": "Example"}
            ],
            "emailAddresses": [
                {"value": "anna@work.example", "type": "work"},
                {"metadata": {"primary": true}, "value": "anna@example.com", "type": "home"},
                {"value": "ANNA@example.com"}
            ],
            "phoneNumbers": [{"value": "+43 1 234", "type": "mobile", "canonicalForm": "+431234"}],
            "organizations": [{"name": "Glasskube", "title": "CTO"}],
            "biographies": [{"value": "Likes tea", "contentType": "TEXT_PLAIN"}],
            "photos": [
                {"metadata": {"primary": true}, "url": "https://lh3.example/p=s100", "default": true},
                {"url": "https://lh3.example/real=s100"}
            ]
        });
        let person = serde_json::from_value::<GPerson>(json)
            .unwrap()
            .into_person();
        assert_eq!(person.resource_name, "people/c1");
        assert_eq!(person.fields.name.full().as_deref(), Some("Anna Example"));
        assert_eq!(
            person.fields.email_addresses(),
            ["anna@example.com", "anna@work.example"]
        );
        assert_eq!(person.fields.emails[0].kind.as_deref(), Some("home"));
        assert_eq!(person.fields.phones[0].kind.as_deref(), Some("mobile"));
        assert_eq!(person.fields.organizations[0].title.as_deref(), Some("CTO"));
        assert_eq!(person.fields.note.as_deref(), Some("Likes tea"));
        assert_eq!(
            person.photo_url.as_deref(),
            Some("https://lh3.example/real=s96")
        );
        assert_eq!(person.sources[0].id, "abc");
        assert!(!person.deleted);
    }

    #[test]
    fn writes_only_editable_fields() {
        let fields = ContactFields {
            name: Name {
                display: Some("Bob Builder".into()),
                ..Name::default()
            },
            emails: vec![Labeled::with_kind("bob@example.com", "work")],
            note: Some("Builds".into()),
            ..ContactFields::default()
        };
        let json = serde_json::to_value(GPerson::from_fields(&fields)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "names": [{"unstructuredName": "Bob Builder"}],
                "emailAddresses": [{"value": "bob@example.com", "type": "work"}],
                "phoneNumbers": [],
                "organizations": [],
                "addresses": [],
                "urls": [],
                "biographies": [{"value": "Builds", "contentType": "TEXT_PLAIN"}]
            })
        );
    }

    #[test]
    fn classifies_errors() {
        let expired = GError::parse(
            br#"{"error":{"code":400,"message":"Sync token is expired. Clear local cache and retry call without the sync token.","status":"FAILED_PRECONDITION","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"EXPIRED_SYNC_TOKEN"}]}}"#,
        );
        assert!(expired.is_expired_sync_token());
        let scope = GError::parse(
            br#"{"error":{"code":403,"message":"Request had insufficient authentication scopes.","status":"PERMISSION_DENIED","details":[{"reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#,
        );
        assert!(scope.is_missing_scope() && !scope.is_api_disabled());
        let disabled = GError::parse(
            br#"{"error":{"code":403,"message":"People API has not been used in project 1 before or it is disabled.","status":"PERMISSION_DENIED","details":[{"reason":"SERVICE_DISABLED"}]}}"#,
        );
        assert!(disabled.is_api_disabled());
        assert!(GError::parse(b"not json").message.is_empty());
    }
}

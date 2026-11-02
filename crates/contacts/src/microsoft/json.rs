//! Microsoft Graph's contact JSON (`contact`, delta pages, errors) and its mapping to
//! and from [`Person`] / [`ContactFields`].
//!
//! Outlook contacts have fixed slots, so writing maps Maze's lists onto them: up to
//! three email addresses; phone numbers labeled `mobile` (or `cell`) to the mobile
//! number, `work` to the business numbers and the rest to the home numbers (two each);
//! addresses by label to the home, business and other address; the first web site to
//! the business home page; the first organization to company, department and job
//! title. What doesn't fit is left out (and logged as a count).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{ContactFields, Labeled, Name, Organization, Person, PostalAddress, non_empty};

/// The contact properties Maze reads (`$select`).
pub const SELECT: &str = "id,changeKey,parentFolderId,displayName,givenName,middleName,\
    surname,title,generation,emailAddresses,businessPhones,homePhones,mobilePhone,companyName,\
    department,jobTitle,businessHomePage,personalNotes,homeAddress,businessAddress,otherAddress";

/// Outlook keeps at most this many email addresses per contact.
pub(crate) const MAX_EMAILS: usize = 3;

/// Outlook keeps at most this many business and this many home phone numbers.
pub(crate) const MAX_PHONES_PER_KIND: usize = 2;

/// The scheme of photo "URLs" of Microsoft contacts: `microsoft-photo:<account
/// uuid>:<contact id>`. Graph serves the photo only with the account's token, so the
/// account is part of the reference.
pub const PHOTO_URL_PREFIX: &str = "microsoft-photo:";

/// The photo reference of contact `id` of `account`.
pub(crate) fn photo_url(account: Uuid, id: &str) -> String {
    format!("{PHOTO_URL_PREFIX}{account}:{id}")
}

/// The account and contact id of a [`PHOTO_URL_PREFIX`] reference.
pub fn parse_photo_url(url: &str) -> Option<(Uuid, &str)> {
    let rest = url.strip_prefix(PHOTO_URL_PREFIX)?;
    let (account, id) = rest.split_once(':')?;
    let account = Uuid::parse_str(account).ok()?;
    is_valid_id(id).then_some((account, id))
}

/// Graph ids are base64 (URL-safe or not): only those characters are accepted, so a
/// stored id can't point a request elsewhere.
pub(crate) fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'=' | b'_' | b'-' | b'+'))
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct MEmail {
    pub(crate) name: Option<String>,
    pub(crate) address: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct MAddress {
    pub(crate) street: Option<String>,
    pub(crate) city: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) postal_code: Option<String>,
    pub(crate) country_or_region: Option<String>,
}

impl MAddress {
    fn is_empty(&self) -> bool {
        [
            &self.street,
            &self.city,
            &self.state,
            &self.postal_code,
            &self.country_or_region,
        ]
        .iter()
        .all(|part| non_empty(part.as_deref()).is_none())
    }

    fn to_postal(&self, kind: &str) -> Option<PostalAddress> {
        (!self.is_empty()).then(|| PostalAddress {
            street: owned(self.street.as_deref()),
            city: owned(self.city.as_deref()),
            region: owned(self.state.as_deref()),
            postal_code: owned(self.postal_code.as_deref()),
            country: owned(self.country_or_region.as_deref()),
            kind: Some(kind.to_owned()),
            ..PostalAddress::default()
        })
    }

    fn from_postal(address: &PostalAddress) -> Self {
        // Outlook has one street field: PO box and extended address go on lines of
        // their own above the street.
        let street: Vec<&str> = [&address.po_box, &address.extended, &address.street]
            .iter()
            .filter_map(|part| non_empty(part.as_deref()))
            .collect();
        Self {
            street: (!street.is_empty()).then(|| street.join("\n")),
            city: owned(address.city.as_deref()),
            state: owned(address.region.as_deref()),
            postal_code: owned(address.postal_code.as_deref()),
            country_or_region: owned(address.country.as_deref()),
        }
    }
}

/// A deleted contact in a delta page.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct MRemoved {
    #[allow(dead_code)]
    pub(crate) reason: Option<String>,
}

/// A contact as Graph sends it.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct MContact {
    pub(crate) id: Option<String>,
    #[serde(rename = "@odata.etag")]
    pub(crate) odata_etag: Option<String>,
    pub(crate) change_key: Option<String>,
    pub(crate) parent_folder_id: Option<String>,
    #[serde(rename = "@removed")]
    pub(crate) removed: Option<MRemoved>,
    pub(crate) display_name: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) middle_name: Option<String>,
    pub(crate) surname: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) generation: Option<String>,
    pub(crate) email_addresses: Vec<MEmail>,
    pub(crate) business_phones: Vec<String>,
    pub(crate) home_phones: Vec<String>,
    pub(crate) mobile_phone: Option<String>,
    pub(crate) company_name: Option<String>,
    pub(crate) department: Option<String>,
    pub(crate) job_title: Option<String>,
    pub(crate) business_home_page: Option<String>,
    pub(crate) personal_notes: Option<String>,
    pub(crate) home_address: Option<MAddress>,
    pub(crate) business_address: Option<MAddress>,
    pub(crate) other_address: Option<MAddress>,
}

impl MContact {
    /// The contact as a [`Person`] of `account`'s book.
    pub(crate) fn into_person(self, account: Uuid) -> Person {
        let id = self.id.clone().unwrap_or_default();
        let deleted = self.removed.is_some();
        let mut phones = Vec::new();
        if let Some(mobile) = non_empty(self.mobile_phone.as_deref()) {
            phones.push(Labeled::with_kind(mobile, "mobile"));
        }
        phones.extend(
            self.business_phones
                .iter()
                .filter_map(|p| non_empty(Some(p)))
                .map(|p| Labeled::with_kind(p, "work")),
        );
        phones.extend(
            self.home_phones
                .iter()
                .filter_map(|p| non_empty(Some(p)))
                .map(|p| Labeled::with_kind(p, "home")),
        );
        let organization = Organization {
            name: owned(self.company_name.as_deref()),
            department: owned(self.department.as_deref()),
            title: owned(self.job_title.as_deref()),
        };
        let has_organization = organization.name.is_some()
            || organization.department.is_some()
            || organization.title.is_some();
        let addresses = [
            (&self.home_address, "home"),
            (&self.business_address, "work"),
            (&self.other_address, "other"),
        ]
        .into_iter()
        .filter_map(|(address, kind)| address.as_ref()?.to_postal(kind))
        .collect();
        let fields = ContactFields {
            name: Name {
                display: owned(self.display_name.as_deref()),
                given: owned(self.given_name.as_deref()),
                family: owned(self.surname.as_deref()),
                middle: owned(self.middle_name.as_deref()),
                prefix: owned(self.title.as_deref()),
                suffix: owned(self.generation.as_deref()),
            },
            emails: self
                .email_addresses
                .iter()
                .filter_map(|e| non_empty(e.address.as_deref()))
                .map(Labeled::new)
                .collect(),
            phones,
            organizations: if has_organization {
                vec![organization]
            } else {
                Vec::new()
            },
            addresses,
            urls: owned(self.business_home_page.as_deref())
                .map(|url| vec![Labeled::with_kind(url, "work")])
                .unwrap_or_default(),
            note: owned(self.personal_notes.as_deref()),
        };
        Person {
            photo_url: (!deleted && is_valid_id(&id)).then(|| photo_url(account, &id)),
            resource_name: id,
            etag: self.odata_etag.or(self.change_key),
            fields,
            sources: Vec::new(),
            deleted,
        }
    }
}

/// A contact as Maze writes it. Every field Maze manages is sent, empty ones as
/// `null` or `[]`: a PATCH leaves fields it doesn't name as they are, so a removed
/// phone number has to be sent as removed.
#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MContactWrite {
    pub(crate) display_name: Option<String>,
    pub(crate) given_name: Option<String>,
    pub(crate) middle_name: Option<String>,
    pub(crate) surname: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) generation: Option<String>,
    pub(crate) email_addresses: Vec<MEmail>,
    pub(crate) business_phones: Vec<String>,
    pub(crate) home_phones: Vec<String>,
    pub(crate) mobile_phone: Option<String>,
    pub(crate) company_name: Option<String>,
    pub(crate) department: Option<String>,
    pub(crate) job_title: Option<String>,
    pub(crate) business_home_page: Option<String>,
    pub(crate) personal_notes: Option<String>,
    pub(crate) home_address: Option<MAddress>,
    pub(crate) business_address: Option<MAddress>,
    pub(crate) other_address: Option<MAddress>,
}

impl MContactWrite {
    /// `fields` mapped onto Outlook's slots (see the [module docs](self)).
    pub(crate) fn from_fields(fields: &ContactFields) -> Self {
        let display_name = fields.name.full();
        let mut dropped = 0usize;

        let emails = fields.email_addresses();
        dropped += emails.len().saturating_sub(MAX_EMAILS);
        let email_addresses = emails
            .into_iter()
            .take(MAX_EMAILS)
            .map(|address| MEmail {
                name: display_name.clone(),
                address: Some(address),
            })
            .collect();

        let mut mobile_phone: Option<String> = None;
        let mut business_phones = Vec::new();
        let mut home_phones = Vec::new();
        for phone in &fields.phones {
            let Some(value) = non_empty(Some(&phone.value)) else {
                continue;
            };
            let kind = phone.kind.as_deref().unwrap_or("").to_ascii_lowercase();
            let is_mobile = matches!(kind.as_str(), "mobile" | "cell");
            if is_mobile && mobile_phone.is_none() {
                mobile_phone = Some(value.to_owned());
                continue;
            }
            let (first, second) = if kind == "work" {
                (&mut business_phones, &mut home_phones)
            } else {
                (&mut home_phones, &mut business_phones)
            };
            if first.len() < MAX_PHONES_PER_KIND {
                first.push(value.to_owned());
            } else if second.len() < MAX_PHONES_PER_KIND {
                second.push(value.to_owned());
            } else if mobile_phone.is_none() {
                mobile_phone = Some(value.to_owned());
            } else {
                dropped += 1;
            }
        }

        let mut home_address = None;
        let mut business_address = None;
        let mut other_address = None;
        for address in &fields.addresses {
            let mapped = MAddress::from_postal(address);
            if mapped.is_empty() {
                continue;
            }
            let kind = address.kind.as_deref().unwrap_or("").to_ascii_lowercase();
            let preferred = match kind.as_str() {
                "home" => 0,
                "work" | "business" => 1,
                _ => 2,
            };
            let slots = [&mut home_address, &mut business_address, &mut other_address];
            // The preferred slot first, then the other address, home and business.
            let order = [preferred, 2, 0, 1];
            match order.into_iter().find(|ix| slots[*ix].is_none()) {
                Some(ix) => *slots[ix] = Some(mapped),
                None => dropped += 1,
            }
        }

        let urls: Vec<&str> = fields
            .urls
            .iter()
            .filter_map(|u| non_empty(Some(&u.value)))
            .collect();
        dropped += urls.len().saturating_sub(1);
        let organization = fields.organizations.first();
        dropped += fields.organizations.len().saturating_sub(1);
        if dropped > 0 {
            tracing::info!(
                dropped,
                "values that Outlook contacts have no field for were left out"
            );
        }
        Self {
            display_name,
            given_name: owned(fields.name.given.as_deref()),
            middle_name: owned(fields.name.middle.as_deref()),
            surname: owned(fields.name.family.as_deref()),
            title: owned(fields.name.prefix.as_deref()),
            generation: owned(fields.name.suffix.as_deref()),
            email_addresses,
            business_phones,
            home_phones,
            mobile_phone,
            company_name: organization.and_then(|o| owned(o.name.as_deref())),
            department: organization.and_then(|o| owned(o.department.as_deref())),
            job_title: organization.and_then(|o| owned(o.title.as_deref())),
            business_home_page: urls.first().map(|u| (*u).to_owned()),
            personal_notes: owned(fields.note.as_deref()),
            home_address,
            business_address,
            other_address,
        }
    }
}

/// A page of a contacts list or delta query.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct MPage {
    pub(crate) value: Vec<MContact>,
    #[serde(rename = "@odata.nextLink")]
    pub(crate) next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    pub(crate) delta_link: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct MErrorBody {
    error: MError,
}

/// Graph's error answer (`{"error": {"code": …, "message": …}}`).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub(crate) struct MError {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl MError {
    pub(crate) fn parse(body: &[u8]) -> Self {
        serde_json::from_slice::<MErrorBody>(body)
            .map(|b| b.error)
            .unwrap_or_default()
    }

    fn code_is(&self, codes: &[&str]) -> bool {
        codes.iter().any(|c| self.code.eq_ignore_ascii_case(c))
    }

    /// A delta token Graph no longer knows (HTTP 410, or 400 with a sync state code).
    pub(crate) fn is_expired_delta(&self) -> bool {
        self.code_is(&["SyncStateNotFound", "SyncStateInvalid", "resyncRequired"])
    }

    /// The token lacks `Contacts.ReadWrite` (or the user withdrew it).
    pub(crate) fn is_access_denied(&self) -> bool {
        self.code_is(&[
            "ErrorAccessDenied",
            "Authorization_RequestDenied",
            "AccessDenied",
        ])
    }

    /// Someone else changed the contact (etag mismatch).
    pub(crate) fn is_conflict(&self) -> bool {
        self.code_is(&["ErrorIrresolvableConflict", "PreconditionFailed"])
    }
}

fn owned(value: Option<&str>) -> Option<String> {
    non_empty(value).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn account() -> Uuid {
        Uuid::from_u128(0x6f00_0000_0000_4000_8000_0000_0000_0001)
    }

    #[test]
    fn a_contact_maps_to_a_person() {
        let contact: MContact = serde_json::from_str(
            r#"{
              "@odata.etag": "W/\"EQAAABYAAAB\"",
              "id": "AAMkAGI2=",
              "changeKey": "EQAAABYAAAB",
              "displayName": "Anna Example",
              "givenName": "Anna",
              "surname": "Example",
              "title": "Dr.",
              "emailAddresses": [{"name": "Anna", "address": "anna@example.com"},
                                 {"name": "Anna", "address": " "}],
              "businessPhones": ["+43 1 234"],
              "homePhones": [],
              "mobilePhone": "+43 660 1",
              "companyName": "Northwind",
              "jobTitle": "Engineer",
              "businessHomePage": "https://northwind.example",
              "personalNotes": "Likes tea",
              "homeAddress": {},
              "businessAddress": {"street": "Ring 1", "city": "Wien", "postalCode": "1010",
                                  "countryOrRegion": "Austria"}
            }"#,
        )
        .unwrap();
        let person = contact.into_person(account());
        assert_eq!(person.resource_name, "AAMkAGI2=");
        assert_eq!(person.etag.as_deref(), Some("W/\"EQAAABYAAAB\""));
        assert_eq!(person.fields.name.full().as_deref(), Some("Anna Example"));
        assert_eq!(person.fields.name.prefix.as_deref(), Some("Dr."));
        assert_eq!(person.fields.email_addresses(), ["anna@example.com"]);
        assert_eq!(
            person.fields.phones,
            [
                Labeled::with_kind("+43 660 1", "mobile"),
                Labeled::with_kind("+43 1 234", "work")
            ]
        );
        assert_eq!(person.fields.organizations.len(), 1);
        assert_eq!(
            person.fields.addresses.len(),
            1,
            "the empty home address is left out"
        );
        assert_eq!(person.fields.addresses[0].kind.as_deref(), Some("work"));
        assert_eq!(person.fields.urls[0].value, "https://northwind.example");
        assert_eq!(person.fields.note.as_deref(), Some("Likes tea"));
        assert_eq!(
            person.photo_url.as_deref(),
            Some("microsoft-photo:6f000000-0000-4000-8000-000000000001:AAMkAGI2=")
        );
        assert!(!person.deleted);
    }

    #[test]
    fn a_removed_contact_is_deleted() {
        let contact: MContact =
            serde_json::from_str(r#"{"id": "AAMk1", "@removed": {"reason": "deleted"}}"#).unwrap();
        let person = contact.into_person(account());
        assert!(person.deleted);
        assert_eq!(person.photo_url, None);
    }

    #[test]
    fn fields_fill_outlooks_slots() {
        let fields = ContactFields {
            name: Name {
                given: Some("Anna".into()),
                family: Some("Example".into()),
                ..Name::default()
            },
            emails: ["a@x.example", "b@x.example", "c@x.example", "d@x.example"]
                .into_iter()
                .map(Labeled::new)
                .collect(),
            phones: vec![
                Labeled::with_kind("1", "work"),
                Labeled::with_kind("2", "mobile"),
                Labeled::with_kind("3", "home"),
                Labeled::new("4"),
                Labeled::with_kind("5", "mobile"),
            ],
            addresses: vec![
                PostalAddress {
                    street: Some("Ring 1".into()),
                    po_box: Some("PO 7".into()),
                    kind: Some("work".into()),
                    ..PostalAddress::default()
                },
                PostalAddress {
                    city: Some("Graz".into()),
                    ..PostalAddress::default()
                },
            ],
            ..ContactFields::default()
        };
        let write = MContactWrite::from_fields(&fields);
        assert_eq!(write.display_name.as_deref(), Some("Anna Example"));
        assert_eq!(write.email_addresses.len(), MAX_EMAILS);
        assert_eq!(write.mobile_phone.as_deref(), Some("2"));
        assert_eq!(write.home_phones, ["3", "4"]);
        // A second mobile number goes where there is room.
        assert_eq!(write.business_phones, ["1", "5"]);
        assert_eq!(
            write
                .business_address
                .as_ref()
                .and_then(|a| a.street.as_deref()),
            Some("PO 7\nRing 1")
        );
        assert_eq!(
            write.other_address.as_ref().and_then(|a| a.city.as_deref()),
            Some("Graz")
        );
        assert_eq!(write.home_address, None);
        // Empty fields are sent as removed.
        let json = serde_json::to_value(&write).unwrap();
        assert_eq!(json["personalNotes"], serde_json::Value::Null);
        assert_eq!(json["homeAddress"], serde_json::Value::Null);
    }

    #[test]
    fn photo_references_round_trip_and_ids_are_checked() {
        let url = photo_url(account(), "AAMk-_=+");
        assert_eq!(parse_photo_url(&url), Some((account(), "AAMk-_=+")));
        assert_eq!(parse_photo_url("https://graph.microsoft.com/x"), None);
        assert_eq!(
            parse_photo_url("microsoft-photo:6f000000-0000-4000-8000-000000000001:../me"),
            None
        );
        assert!(!is_valid_id("a/b") && !is_valid_id("") && !is_valid_id("a?b"));
    }

    #[test]
    fn errors_are_classified() {
        let expired = MError::parse(br#"{"error":{"code":"SyncStateNotFound","message":"x"}}"#);
        assert!(expired.is_expired_delta());
        let denied = MError::parse(br#"{"error":{"code":"ErrorAccessDenied","message":"x"}}"#);
        assert!(denied.is_access_denied());
        assert!(!MError::parse(b"not json").is_conflict());
    }
}

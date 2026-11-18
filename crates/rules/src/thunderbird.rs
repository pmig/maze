//! Thunderbird filters (`msgFilterRules.dat`) in Maze's format.
//!
//! The import parses the file and calls [`term`], [`condition`] and [`action`]. Rules
//! the import stored before the rules engine existed (Thunderbird's terms and action
//! names in the JSON) are converted the same way when they are read.

use serde_json::Value;

use crate::{Action, ActionKind, Condition, Field, Match, Op, Term};

/// The marker of a field Maze doesn't evaluate: `thunderbird:<attribute>`.
const OTHER: &str = "thunderbird:";

/// One search term. `attribute` is Thunderbird's (`from`, `to or cc`, `has attachment
/// status`, …) or, with `custom_header`, a header name.
pub fn term(attribute: &str, custom_header: bool, operator: &str, value: &str) -> Term {
    let lower = attribute.trim().to_lowercase();
    let field = if custom_header {
        match lower.as_str() {
            "list-id" => Field::ListId,
            _ => Field::Other(format!("{OTHER}header {}", attribute.trim())),
        }
    } else {
        match lower.as_str() {
            "from" => Field::From,
            "to" => Field::To,
            "cc" => Field::Cc,
            "to or cc" => Field::ToOrCc,
            "all addresses" => Field::AnyAddress,
            "subject" => Field::Subject,
            "body" => Field::Body,
            "has attachment status" => Field::HasAttachment,
            _ => Field::Other(format!("{OTHER}{lower}")),
        }
    };
    let op = match operator.trim() {
        "contains" => Op::Contains,
        "doesn't contain" => Op::NotContains,
        "is" => Op::Is,
        "isn't" => Op::IsNot,
        "begins with" => Op::BeginsWith,
        "ends with" => Op::EndsWith,
        other => Op::Other(format!("{OTHER}{other}")),
    };
    Term::new(field, op, value)
}

/// A filter's condition: `match_all` is Thunderbird's `ALL`; each term comes with
/// whether it joins with `OR`. All `OR` is "any", all `AND` is "all"; a mix (Thunderbird
/// only writes one with grouping Maze doesn't have) gets a term Maze can't evaluate, so
/// the rule can't be enabled.
pub fn condition(match_all: bool, terms: Vec<(bool, Term)>) -> Condition {
    if match_all {
        return Condition {
            matching: Match::AllMessages,
            terms: Vec::new(),
        };
    }
    let ors = terms.iter().filter(|(or, _)| *or).count();
    let mixed = ors > 0 && ors < terms.len();
    let mut terms: Vec<Term> = terms.into_iter().map(|(_, term)| term).collect();
    if mixed {
        terms.push(Term::new(
            Field::Other(format!("{OTHER}grouped and/or")),
            Op::Is,
            "",
        ));
    }
    Condition {
        matching: if ors > 0 && !mixed {
            Match::Any
        } else {
            Match::All
        },
        terms,
    }
}

/// One action, by Thunderbird's name (`Move to folder`, `AddTag`, …).
pub fn action(name: &str, value: Option<&str>) -> Action {
    let kind = match name {
        "Move to folder" => ActionKind::MoveToFolder,
        "Copy to folder" => ActionKind::CopyToFolder,
        "AddTag" => ActionKind::AddTag,
        "Mark read" => ActionKind::MarkRead,
        "Mark unread" => ActionKind::MarkUnread,
        "Mark flagged" => ActionKind::MarkFlagged,
        // Thunderbird's Delete moves to the Trash (unless the account deletes for good;
        // Maze's rules never do).
        "Delete" => ActionKind::Trash,
        "Stop execution" => ActionKind::Stop,
        "JunkScore" if value.map(str::trim) == Some("100") => ActionKind::Junk,
        other => ActionKind::Other(other.to_lowercase().replace(char::is_whitespace, "_")),
    };
    let value = match kind {
        ActionKind::MoveToFolder | ActionKind::CopyToFolder => {
            value.map(|v| folder_path(v).unwrap_or_else(|| v.to_owned()))
        }
        ActionKind::AddTag => value.map(str::to_owned),
        ActionKind::Other(_) => value.map(str::to_owned),
        _ => None,
    };
    Action { kind, value }
}

/// The folder path of a Thunderbird folder URI
/// (`imap://anna%40example.com@imap.example.com/INBOX/Lists` → `INBOX/Lists`), or
/// `None` if `uri` isn't one.
pub fn folder_path(uri: &str) -> Option<String> {
    let (_, rest) = uri.split_once("://")?;
    let (_, path) = rest.split_once('/')?;
    (!path.is_empty()).then(|| percent_decode(path))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A condition as the import stored it before the rules engine: Thunderbird's terms
/// (`boolean`, `attribute`, `custom_header`, `operator`, `value`).
pub(crate) fn legacy_condition(terms: &[Value]) -> Result<Condition, String> {
    let terms = terms
        .iter()
        .map(|t| {
            let text = |key: &str| t.get(key).and_then(Value::as_str).unwrap_or_default();
            let attribute = t
                .get("attribute")
                .and_then(Value::as_str)
                .ok_or("a term has no `attribute`")?;
            let custom = t
                .get("custom_header")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok((
                text("boolean") == "or",
                term(attribute, custom, text("operator"), text("value")),
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(condition(false, terms))
}

/// An action as the import stored it before the rules engine (`delete`,
/// `junk_score`, …, see `import::thunderbird::filters`).
pub(crate) fn legacy_action(name: &str, value: Option<&str>) -> Action {
    match name {
        "delete" => Action::new(ActionKind::Trash),
        "junk_score" if value.map(str::trim) == Some("100") => Action::new(ActionKind::Junk),
        other => Action {
            kind: ActionKind::Other(other.to_owned()),
            value: value.map(str::to_owned),
        },
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::Rule;

    #[test]
    fn terms_and_conditions() {
        assert_eq!(
            term("to or cc", false, "doesn't contain", "me"),
            Term::new(Field::ToOrCc, Op::NotContains, "me")
        );
        assert_eq!(term("List-Id", true, "is", "x").field, Field::ListId);
        assert!(!term("X-Spam-Flag", true, "is", "YES").is_supported());
        assert!(!term("age in days", false, "is greater than", "3").is_supported());
        assert!(!term("subject", false, "matches", "x.*").is_supported());

        let a = term("subject", false, "contains", "a");
        let b = term("from", false, "is", "b@example.com");
        let any = condition(false, vec![(true, a.clone()), (true, b.clone())]);
        assert_eq!(any.matching, Match::Any);
        let all = condition(false, vec![(false, a.clone()), (false, b.clone())]);
        assert_eq!(all.matching, Match::All);
        let mixed = condition(false, vec![(false, a), (true, b)]);
        assert_eq!(mixed.terms.len(), 3);
        assert!(!mixed.terms[2].is_supported());
        assert_eq!(condition(true, Vec::new()).matching, Match::AllMessages);
    }

    #[test]
    fn actions_and_folder_uris() {
        assert_eq!(
            action(
                "Move to folder",
                Some("imap://anna%40example.com@imap.example.com/INBOX/Lists%20Dev")
            ),
            Action::with_value(ActionKind::MoveToFolder, "INBOX/Lists Dev")
        );
        assert_eq!(action("Delete", None), Action::new(ActionKind::Trash));
        assert_eq!(action("JunkScore", Some("100")).kind, ActionKind::Junk);
        assert_eq!(
            action("JunkScore", Some("0")).kind,
            ActionKind::Other("junkscore".into())
        );
        assert_eq!(
            action("Reply", Some("t")).kind,
            ActionKind::Other("reply".into())
        );
        assert_eq!(folder_path("Lists"), None);
    }

    #[test]
    fn reads_what_the_import_stored_before_the_rules_engine() {
        let rule = Rule::parse(
            r#"{"source":"thunderbird","match":"terms","terms":[
                 {"boolean":"or","attribute":"list-id","custom_header":true,"operator":"contains","value":"dev"},
                 {"boolean":"or","attribute":"subject","custom_header":false,"operator":"contains","value":"x"}],
               "raw":"…"}"#,
            r#"[{"action":"move_to_folder","value":"imap://a%40b.c@imap.b.c/Lists","supported":true},
                {"action":"delete","value":null},{"action":"forward","value":"x@y.z"}]"#,
        )
        .unwrap();
        assert_eq!(rule.condition.matching, Match::Any);
        assert_eq!(rule.condition.terms[0].field, Field::ListId);
        assert_eq!(
            rule.actions,
            vec![
                Action::with_value(ActionKind::MoveToFolder, "Lists"),
                Action::new(ActionKind::Trash),
                Action::with_value(ActionKind::Other("forward".into()), "x@y.z"),
            ]
        );
    }
}

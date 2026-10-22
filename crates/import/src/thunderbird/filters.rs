//! `msgFilterRules.dat` (PLAN §4.13, `mailnews/search/src/nsMsgFilterList.cpp`).
//!
//! The file is a sequence of `attribute="value"` lines: a `version` and `logging`
//! header, then per filter `name`, `enabled`, `description`, `type`, and any number of
//! `action`/`actionValue`/`customId` groups followed by one `condition`. Values escape
//! `"` and `\` with a backslash. Conditions are `ALL` or a list of `AND (attr,op,value)`
//! / `OR (…)` terms; a quoted attribute is an arbitrary header, a quoted value may
//! contain `)` and `,`.

use serde::Serialize;

use store::RuleTrigger;

/// Filter type bits (`nsMsgFilterType`, `nsMsgFilterCore.idl`).
pub mod filter_type {
    /// Getting new mail, before junk classification.
    pub const INBOX_RULE: i64 = 0x01;
    /// Run manually.
    pub const MANUAL: i64 = 0x10;
    /// Getting new mail, after junk classification.
    pub const POST_PLUGIN: i64 = 0x20;
    /// After sending.
    pub const POST_OUTGOING: i64 = 0x40;
    /// When archiving.
    pub const ARCHIVE: i64 = 0x80;
    /// Periodically.
    pub const PERIODIC: i64 = 0x100;
}

/// A whole filter file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FilterFile {
    pub version: Option<i64>,
    pub logging: bool,
    pub filters: Vec<Filter>,
    /// Lines that couldn't be read (skipped).
    pub errors: usize,
}

/// One filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Filter {
    pub name: String,
    pub enabled: bool,
    pub description: Option<String>,
    /// [`filter_type`] bits; Thunderbird's default is "new mail" + "manual".
    pub filter_type: i64,
    pub script_name: Option<String>,
    pub actions: Vec<FilterAction>,
    pub condition: Condition,
}

/// One action.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FilterAction {
    /// As written (`Move to folder`, `AddTag`, `Custom`, …).
    pub action: String,
    pub value: Option<String>,
    /// The extension id of a `Custom` action.
    pub custom_id: Option<String>,
}

impl FilterAction {
    /// Whether Maze's rules engine will be able to run this action. Custom actions,
    /// replies and forwards with templates, JavaScript and POP3 actions are not.
    pub fn is_supported(&self) -> bool {
        matches!(
            self.action.as_str(),
            "Move to folder"
                | "Copy to folder"
                | "Change priority"
                | "Delete"
                | "Mark read"
                | "Mark unread"
                | "Ignore thread"
                | "Ignore subthread"
                | "Watch thread"
                | "Mark flagged"
                | "Stop execution"
                | "JunkScore"
                | "AddTag"
                | "Forward"
        )
    }

    /// A stable snake_case name for the action JSON.
    pub fn kind(&self) -> String {
        match self.action.as_str() {
            "Move to folder" => "move_to_folder".to_owned(),
            "Copy to folder" => "copy_to_folder".to_owned(),
            "Change priority" => "change_priority".to_owned(),
            "Delete" => "delete".to_owned(),
            "Mark read" => "mark_read".to_owned(),
            "Mark unread" => "mark_unread".to_owned(),
            "Ignore thread" => "ignore_thread".to_owned(),
            "Ignore subthread" => "ignore_subthread".to_owned(),
            "Watch thread" => "watch_thread".to_owned(),
            "Mark flagged" => "mark_flagged".to_owned(),
            "Reply" => "reply".to_owned(),
            "Forward" => "forward".to_owned(),
            "Stop execution" => "stop".to_owned(),
            "Delete from Pop3 server" => "pop3_delete".to_owned(),
            "Leave on Pop3 server" => "pop3_leave".to_owned(),
            "JunkScore" => "junk_score".to_owned(),
            "Fetch body from Pop3Server" => "pop3_fetch_body".to_owned(),
            "AddTag" => "add_tag".to_owned(),
            "Custom" => "custom".to_owned(),
            other => format!(
                "unknown:{}",
                other.to_lowercase().replace(char::is_whitespace, "_")
            ),
        }
    }
}

/// A filter's condition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Condition {
    /// `ALL`: matches every message.
    pub match_all: bool,
    pub terms: Vec<Term>,
    /// The condition as written, for the record.
    pub raw: String,
}

/// One search term.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Term {
    /// `and` or `or`: how this term joins the ones before it.
    pub boolean: BoolOp,
    /// `subject`, `from`, `body`, `tag`, … or an arbitrary header name.
    pub attribute: String,
    /// The attribute is an arbitrary header (`"X-Spam-Flag"`).
    pub custom_header: bool,
    pub operator: String,
    pub value: String,
}

/// How a term joins the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BoolOp {
    And,
    Or,
}

/// Parses the text of a `msgFilterRules.dat`.
pub fn parse(text: &str) -> FilterFile {
    let mut file = FilterFile::default();
    let mut current: Option<Filter> = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((attr, value)) = parse_line(line) else {
            file.errors += 1;
            continue;
        };
        match attr {
            "version" => file.version = value.trim().parse().ok(),
            "logging" => file.logging = value == "yes",
            "name" => {
                if let Some(done) = current.take() {
                    file.filters.push(done);
                }
                current = Some(Filter {
                    name: value,
                    enabled: true,
                    filter_type: filter_type::INBOX_RULE | filter_type::MANUAL,
                    ..Filter::default()
                });
            }
            _ => {
                let Some(filter) = current.as_mut() else {
                    file.errors += 1;
                    continue;
                };
                match attr {
                    "enabled" => filter.enabled = value == "yes",
                    "description" => filter.description = Some(value).filter(|v| !v.is_empty()),
                    "type" => {
                        if let Ok(t) = value.trim().parse() {
                            filter.filter_type = t;
                        }
                    }
                    "scriptName" => filter.script_name = Some(value),
                    "action" => filter.actions.push(FilterAction {
                        action: value,
                        ..FilterAction::default()
                    }),
                    "actionValue" => match filter.actions.last_mut() {
                        Some(action) => action.value = Some(value),
                        None => file.errors += 1,
                    },
                    "customId" => match filter.actions.last_mut() {
                        Some(action) => action.custom_id = Some(value),
                        None => file.errors += 1,
                    },
                    "condition" => filter.condition = parse_condition(&value),
                    _ => file.errors += 1,
                }
            }
        }
    }
    if let Some(done) = current {
        file.filters.push(done);
    }
    file
}

/// `attr="value"` with `\"` and `\\` unescaped.
fn parse_line(line: &str) -> Option<(&str, String)> {
    let (attr, rest) = line.split_once('=')?;
    let rest = rest.trim().strip_prefix('"')?;
    let mut value = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('"') => value.push('"'),
                Some('\\') => value.push('\\'),
                Some(other) => {
                    value.push('\\');
                    value.push(other);
                }
                None => value.push('\\'),
            },
            '"' => break,
            c => value.push(c),
        }
    }
    Some((attr.trim(), value))
}

/// Parses a condition string (see the module docs).
pub fn parse_condition(raw: &str) -> Condition {
    let mut condition = Condition {
        raw: raw.to_owned(),
        ..Condition::default()
    };
    if raw.trim() == "ALL" {
        condition.match_all = true;
        return condition;
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut pos = 0;
    while pos < chars.len() {
        let Some(open) = chars[pos..].iter().position(|&c| c == '(').map(|i| pos + i) else {
            break;
        };
        let prefix: String = chars[pos..open].iter().collect();
        let boolean = if prefix.trim().eq_ignore_ascii_case("OR") {
            BoolOp::Or
        } else {
            BoolOp::And
        };
        // Find the closing paren outside quotes; `\"` doesn't toggle quoting.
        let mut in_quote = false;
        let mut end = None;
        let mut i = open + 1;
        while i < chars.len() {
            match chars[i] {
                '\\' if chars.get(i + 1) == Some(&'"') => i += 1,
                '"' => in_quote = !in_quote,
                ')' if !in_quote => {
                    end = Some(i);
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        let Some(end) = end else {
            break;
        };
        let inner: String = chars[open + 1..end].iter().collect();
        if let Some(term) = parse_term(&inner.replace("\\\"", "\""), boolean) {
            condition.terms.push(term);
        }
        pos = end + 1;
    }
    condition
}

fn parse_term(inner: &str, boolean: BoolOp) -> Option<Term> {
    let (attribute, custom_header, rest) = if let Some(quoted) = inner.strip_prefix('"') {
        let (name, rest) = quoted.split_once('"')?;
        (name.to_owned(), true, rest.strip_prefix(',')?)
    } else {
        let (name, rest) = inner.split_once(',')?;
        (name.trim().to_owned(), false, rest)
    };
    let (operator, value) = match rest.split_once(',') {
        Some((op, value)) => (op.trim().to_owned(), value),
        None => (rest.trim().to_owned(), ""),
    };
    let value = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
        .to_owned();
    Some(Term {
        boolean,
        attribute,
        custom_header,
        operator,
        value,
    })
}

impl Filter {
    /// The trigger Maze stores (one per rule; all of Thunderbird's are kept in the
    /// condition JSON): new mail before junk classification, after it, manual, after
    /// sending, periodic — in that order of preference.
    pub fn primary_trigger(&self) -> RuleTrigger {
        let t = self.filter_type;
        if t & filter_type::INBOX_RULE != 0 {
            RuleTrigger::ArrivalBeforeJunk
        } else if t & filter_type::POST_PLUGIN != 0 {
            RuleTrigger::Arrival
        } else if t & filter_type::POST_OUTGOING != 0 {
            RuleTrigger::AfterSend
        } else if t & filter_type::PERIODIC != 0 {
            RuleTrigger::Periodic
        } else {
            RuleTrigger::Manual
        }
    }

    /// All triggers the type bits name.
    pub fn triggers(&self) -> Vec<&'static str> {
        let t = self.filter_type;
        [
            (filter_type::INBOX_RULE, "arrival_before_junk"),
            (filter_type::POST_PLUGIN, "arrival"),
            (filter_type::MANUAL, "manual"),
            (filter_type::POST_OUTGOING, "after_send"),
            (filter_type::ARCHIVE, "archive"),
            (filter_type::PERIODIC, "periodic"),
        ]
        .into_iter()
        .filter(|(bit, _)| t & bit != 0)
        .map(|(_, name)| name)
        .collect()
    }

    /// Actions Maze can't run yet.
    pub fn unsupported_actions(&self) -> Vec<&FilterAction> {
        self.actions.iter().filter(|a| !a.is_supported()).collect()
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_filters_actions_and_conditions() {
        let file = parse(
            r#"version="9"
logging="yes"
name="Newsletters"
enabled="yes"
type="17"
action="Move to folder"
actionValue="imap://me%40example.com@imap.example.com/Lists"
action="Mark read"
condition="OR (list-id,contains,dev.example.org) OR (\"X-Mailer\",is,Mass Mailer) OR (subject,contains,\"a, (b)\")"
name="Off"
enabled="no"
type="64"
action="Custom"
customId="filtaquilla@mesquilla.com#runFile"
actionValue="/bin/true"
condition="ALL"
"#,
        );
        assert_eq!(file.version, Some(9));
        assert!(file.logging);
        assert_eq!(file.filters.len(), 2);
        let news = &file.filters[0];
        assert!(news.enabled);
        assert_eq!(news.actions.len(), 2);
        assert_eq!(news.actions[0].kind(), "move_to_folder");
        assert_eq!(news.condition.terms.len(), 3);
        assert_eq!(news.condition.terms[0].boolean, BoolOp::Or);
        assert!(news.condition.terms[1].custom_header);
        assert_eq!(news.condition.terms[1].attribute, "X-Mailer");
        assert_eq!(news.condition.terms[2].value, "a, (b)");
        assert_eq!(news.primary_trigger(), RuleTrigger::ArrivalBeforeJunk);
        assert_eq!(news.triggers(), vec!["arrival_before_junk", "manual"]);
        let off = &file.filters[1];
        assert!(!off.enabled);
        assert!(off.condition.match_all);
        assert_eq!(off.primary_trigger(), RuleTrigger::AfterSend);
        assert_eq!(off.unsupported_actions().len(), 1);
        assert_eq!(
            off.actions[0].custom_id.as_deref(),
            Some("filtaquilla@mesquilla.com#runFile")
        );
    }

    #[test]
    fn tolerates_garbage() {
        let file = parse("junk\nactionValue=\"x\"\nname=\"a\"\nbogus=\"1\"\nactionValue=\"y\"\n");
        assert_eq!(file.filters.len(), 1);
        assert_eq!(file.errors, 4);
        assert!(parse_condition("AND (subject,contains").terms.is_empty());
    }
}

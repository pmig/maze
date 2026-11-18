//! Message rules (filters, ADR-029): the condition and action format stored in the
//! store's `rule` table, matching a message against a condition, and the plan of
//! actions the rules of an account produce for one message.
//!
//! The crate does no IO. `mazemail_core` reads the rules, builds a [`Message`] from the
//! store and runs the planned actions as ordinary mail commands; the Thunderbird import
//! converts `msgFilterRules.dat` filters with [`thunderbird`].
//!
//! # Format
//!
//! The condition (`condition_json`):
//!
//! ```json
//! { "match": "all", "terms": [ { "field": "from", "op": "contains", "value": "anna" } ] }
//! ```
//!
//! `match` is `all`, `any` or `all_messages` (every message, no terms). Fields are
//! [`Field`]s, operators [`Op`]s. Other keys are kept in the stored JSON and ignored
//! here (the import keeps the Thunderbird original next to the terms).
//!
//! The actions (`actions_json`), run in this order per rule (Thunderbird's): flags and
//! tags first, then copies, then the one action that moves the message:
//!
//! ```json
//! [ { "action": "add_tag", "value": "$label1" }, { "action": "move_to_folder", "value": "Lists" } ]
//! ```
//!
//! Folder values are folder paths in the message's account (`Lists`, `INBOX/Lists`);
//! tag values are IMAP keywords or tag names.

mod action;
mod condition;
mod plan;
pub mod thunderbird;

pub use action::{Action, ActionKind};
pub use condition::{Condition, Field, Match, Message, Op, Term};
pub use plan::{Planned, plan};

use serde_json::Value;

/// A stored rule's JSON that can't be read at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// The condition isn't valid JSON of the expected shape.
    #[error("the rule's condition can't be read: {0}")]
    Condition(String),
    /// The actions aren't valid JSON of the expected shape.
    #[error("the rule's actions can't be read: {0}")]
    Actions(String),
}

/// A rule's condition and actions, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// When the rule matches.
    pub condition: Condition,
    /// What it does, as written.
    pub actions: Vec<Action>,
}

impl Rule {
    /// Parses the stored JSON of a rule.
    ///
    /// # Errors
    ///
    /// [`FormatError`] if either part isn't JSON of the expected shape. Unknown fields,
    /// operators and actions are not errors; they show up in [`Rule::problems`].
    pub fn parse(condition_json: &str, actions_json: &str) -> Result<Self, FormatError> {
        let condition: Value = serde_json::from_str(condition_json)
            .map_err(|err| FormatError::Condition(err.to_string()))?;
        let actions: Value = serde_json::from_str(actions_json)
            .map_err(|err| FormatError::Actions(err.to_string()))?;
        Ok(Self {
            condition: Condition::from_json(&condition).map_err(FormatError::Condition)?,
            actions: action::from_json(&actions).map_err(FormatError::Actions)?,
        })
    }

    /// The condition as stored JSON.
    pub fn condition_json(&self) -> String {
        self.condition_json_value().to_string()
    }

    /// The condition as a JSON object (to add keys before storing it).
    pub fn condition_json_value(&self) -> Value {
        self.condition.to_json()
    }

    /// The actions as stored JSON.
    pub fn actions_json(&self) -> String {
        Value::Array(self.actions_json_values()).to_string()
    }

    /// The actions as JSON objects, in order.
    pub fn actions_json_values(&self) -> Vec<Value> {
        self.actions.iter().map(Action::to_json).collect()
    }

    /// What keeps the rule from running as intended. Problems that
    /// [block](Problem::blocks) it mean it may not be enabled.
    pub fn problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();
        match self.condition.matching {
            Match::AllMessages => {}
            Match::All | Match::Any if self.condition.terms.is_empty() => {
                problems.push(Problem::NoConditions);
            }
            Match::All | Match::Any => {}
        }
        if self.condition.matching != Match::AllMessages {
            for term in &self.condition.terms {
                if !term.is_supported() {
                    problems.push(Problem::UnsupportedCondition(term.to_string()));
                }
            }
        }
        let mut runnable = 0;
        for action in &self.actions {
            match &action.kind {
                ActionKind::Other(name) => problems.push(Problem::UnsupportedAction(name.clone())),
                kind if kind.needs_value() && action.value().is_none() => {
                    problems.push(Problem::MissingValue(kind.as_str().to_owned()));
                }
                ActionKind::Stop => {}
                _ => runnable += 1,
            }
        }
        if runnable == 0 {
            problems.push(Problem::NoActions);
        }
        problems
    }

    /// Whether the rule may be enabled: no [blocking](Problem::blocks) problems.
    pub fn is_runnable(&self) -> bool {
        self.problems().iter().all(|p| !p.blocks())
    }

    /// Whether matching needs the message's text (a `body` condition).
    pub fn needs_body(&self) -> bool {
        self.condition.needs_body()
    }

    /// Whether the rule matches `message`.
    pub fn matches(&self, message: &Message<'_>) -> bool {
        self.condition.matches(message)
    }

    /// One line for lists: `from contains "anna" → move to Lists, mark read`.
    pub fn describe(&self) -> String {
        let actions: Vec<String> = self.actions.iter().map(ToString::to_string).collect();
        format!("{} → {}", self.condition, actions.join(", "))
    }
}

/// Something that keeps a rule from running as intended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// `all` or `any` without terms.
    NoConditions,
    /// A condition Maze can't evaluate (a Thunderbird attribute or operator it doesn't
    /// know). Blocks: the rule would match differently than in Thunderbird.
    UnsupportedCondition(String),
    /// No action Maze can run.
    NoActions,
    /// An action Maze doesn't run (reply, forward, custom actions, …); the others run.
    UnsupportedAction(String),
    /// An action without its folder or tag.
    MissingValue(String),
}

impl Problem {
    /// Whether the rule may not be enabled because of it.
    pub fn blocks(&self) -> bool {
        !matches!(self, Self::UnsupportedAction(_))
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoConditions => f.write_str("it has no conditions"),
            Self::UnsupportedCondition(term) => {
                write!(f, "Maze can't evaluate the condition `{term}`")
            }
            Self::NoActions => f.write_str("it has no action Maze can run"),
            Self::UnsupportedAction(name) => {
                write!(f, "the action `{name}` isn't supported and is skipped")
            }
            Self::MissingValue(name) => write!(f, "the action `{name}` has no folder or tag"),
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_and_writes_the_canonical_format() {
        let rule = Rule::parse(
            r#"{"match":"any","terms":[{"field":"from","op":"contains","value":"anna"},
               {"field":"list_id","op":"is","value":"dev.example.org"}],"note":"kept"}"#,
            r#"[{"action":"add_tag","value":"$label1"},{"action":"move_to_folder","value":"Lists"},
               {"action":"stop"}]"#,
        )
        .unwrap();
        assert_eq!(rule.condition.matching, Match::Any);
        assert_eq!(rule.condition.terms[1].field, Field::ListId);
        assert_eq!(rule.actions[1].kind, ActionKind::MoveToFolder);
        assert!(rule.is_runnable());
        let again = Rule::parse(&rule.condition_json(), &rule.actions_json()).unwrap();
        assert_eq!(again, rule);
        assert_eq!(
            rule.describe(),
            "from contains \"anna\" or list-id is \"dev.example.org\" → tag $label1, move to \
             Lists, stop"
        );
    }

    #[test]
    fn problems_block_unsupported_conditions_but_not_unsupported_actions() {
        let rule = Rule::parse(
            r#"{"match":"all","terms":[{"field":"thunderbird:age in days","op":"is greater than","value":"3"}]}"#,
            r#"[{"action":"forward","value":"x@example.com"},{"action":"mark_read"}]"#,
        )
        .unwrap();
        let problems = rule.problems();
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].blocks());
        assert!(!problems[1].blocks());
        assert!(!rule.is_runnable());

        let empty = Rule::parse(r#"{"match":"all","terms":[]}"#, r#"[{"action":"stop"}]"#).unwrap();
        assert_eq!(
            empty.problems(),
            vec![Problem::NoConditions, Problem::NoActions]
        );
        let missing = Rule::parse(
            r#"{"match":"all_messages"}"#,
            r#"[{"action":"move_to_folder"}]"#,
        )
        .unwrap();
        assert_eq!(
            missing.problems(),
            vec![
                Problem::MissingValue("move_to_folder".into()),
                Problem::NoActions
            ]
        );
        assert!(Rule::parse("{", "[]").is_err());
        assert!(Rule::parse(r#"{"match":"sometimes"}"#, "[]").is_err());
    }
}

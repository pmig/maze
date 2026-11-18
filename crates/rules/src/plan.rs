//! The actions the rules of an account produce for one message.

use crate::{Action, ActionKind, Message, Rule};

/// One action to run, with the rule it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned<'a, Id> {
    /// The rule (for the audit log).
    pub rule: Id,
    /// The action.
    pub action: &'a Action,
}

/// The actions for `message`, given `rules` in execution order (Thunderbird's
/// semantics):
///
/// - each matching rule contributes its actions: flags and tags first, then copies,
///   then the first action that moves the message (later ones are dropped);
/// - a rule with `stop`, or one that moved the message, ends the evaluation;
/// - unsupported actions are skipped.
pub fn plan<'a, Id: Copy>(
    rules: impl IntoIterator<Item = (Id, &'a Rule)>,
    message: &Message<'_>,
) -> Vec<Planned<'a, Id>> {
    let mut out = Vec::new();
    for (id, rule) in rules {
        if !rule.matches(message) {
            continue;
        }
        let mut actions: Vec<&Action> = rule
            .actions
            .iter()
            .filter(|a| !matches!(a.kind, ActionKind::Other(_) | ActionKind::Stop))
            .filter(|a| !a.kind.needs_value() || a.value().is_some())
            .collect();
        actions.sort_by_key(|a| a.kind.rank());
        let mut moved = false;
        for action in actions {
            if action.kind.moves() {
                if moved {
                    continue;
                }
                moved = true;
            }
            out.push(Planned { rule: id, action });
        }
        let stop = rule.actions.iter().any(|a| a.kind == ActionKind::Stop);
        if stop || moved {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::{Condition, Field, Match, Op, Term};

    fn rule(condition: Condition, actions: Vec<Action>) -> Rule {
        Rule { condition, actions }
    }

    fn subject(value: &str) -> Condition {
        Condition::all(vec![Term::new(Field::Subject, Op::Contains, value)])
    }

    fn kinds(planned: &[Planned<'_, u32>]) -> Vec<(u32, String)> {
        planned
            .iter()
            .map(|p| (p.rule, p.action.to_string()))
            .collect()
    }

    #[test]
    fn rules_run_in_order_and_moves_come_last() {
        let rules = [
            rule(
                subject("invoice"),
                vec![
                    Action::with_value(ActionKind::MoveToFolder, "Invoices"),
                    Action::new(ActionKind::MarkRead),
                    Action::new(ActionKind::Archive),
                    Action::with_value(ActionKind::CopyToFolder, "Backup"),
                ],
            ),
            rule(
                subject("invoice"),
                vec![Action::new(ActionKind::MarkFlagged)],
            ),
        ];
        let message = Message {
            subject: Some("Invoice 42"),
            ..Message::default()
        };
        let planned = plan(
            rules.iter().enumerate().map(|(i, r)| (i as u32, r)),
            &message,
        );
        // The move ends the evaluation: rule 1 doesn't run; the archive is dropped.
        assert_eq!(
            kinds(&planned),
            vec![
                (0, "mark read".to_owned()),
                (0, "copy to Backup".to_owned()),
                (0, "move to Invoices".to_owned()),
            ]
        );
    }

    #[test]
    fn stop_ends_the_evaluation_and_non_matching_rules_are_skipped() {
        let rules = [
            rule(subject("receipt"), vec![Action::new(ActionKind::Trash)]),
            rule(
                subject("newsletter"),
                vec![
                    Action::with_value(ActionKind::AddTag, "$label4"),
                    Action::new(ActionKind::Stop),
                ],
            ),
            rule(
                Condition {
                    matching: Match::AllMessages,
                    terms: Vec::new(),
                },
                vec![Action::new(ActionKind::MarkRead)],
            ),
        ];
        let news = Message {
            subject: Some("Weekly newsletter"),
            ..Message::default()
        };
        let planned = plan(rules.iter().enumerate().map(|(i, r)| (i as u32, r)), &news);
        assert_eq!(kinds(&planned), vec![(1, "tag $label4".to_owned())]);

        let other = Message {
            subject: Some("Hello"),
            ..Message::default()
        };
        let planned = plan(rules.iter().enumerate().map(|(i, r)| (i as u32, r)), &other);
        assert_eq!(kinds(&planned), vec![(2, "mark read".to_owned())]);
    }

    #[test]
    fn unsupported_and_incomplete_actions_are_skipped() {
        let rules = [rule(
            subject("x"),
            vec![
                Action::with_value(ActionKind::Other("forward".into()), "a@example.com"),
                Action::new(ActionKind::AddTag),
                Action::new(ActionKind::MarkUnread),
            ],
        )];
        let message = Message {
            subject: Some("x"),
            ..Message::default()
        };
        let planned = plan(rules.iter().map(|r| (7_u32, r)), &message);
        assert_eq!(kinds(&planned), vec![(7, "mark unread".to_owned())]);
    }
}

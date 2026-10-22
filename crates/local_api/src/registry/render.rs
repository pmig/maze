//! Human-readable output for terminals (without `--json`).

use serde_json::Value;

use super::cli::RenderOptions;
use super::{Column, CommandSpec, Human};

/// The widest a table cell gets before it is cut with `…`.
const MAX_CELL: usize = 60;

/// Renders a command's output for people, following the spec's [`Human`] layout.
/// Data that doesn't match the layout falls back to pretty JSON.
pub fn render_human(spec: &CommandSpec, data: &Value, options: &RenderOptions) -> String {
    if options.ids_only
        && let Value::Array(rows) = data
    {
        return rows
            .iter()
            .filter_map(|row| row.get("id").and_then(Value::as_str))
            .map(|id| format!("{id}\n"))
            .collect();
    }
    match (spec.human, data) {
        (Human::Table(columns), Value::Array(rows)) => table(columns, rows),
        (Human::Field(field), Value::Object(object)) => match object.get(field) {
            Some(Value::String(text)) => with_newline(text.clone()),
            Some(other) => with_newline(cell(other)),
            None => fields(data),
        },
        (Human::Fields, Value::Object(_)) => fields(data),
        (_, Value::Null) => String::new(),
        _ => with_newline(serde_json::to_string_pretty(data).unwrap_or_default()),
    }
}

fn with_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn lookup<'a>(row: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(row, |value, key| value.get(key))
}

/// A scalar as text; arrays joined with commas; objects as JSON.
fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items.iter().map(cell).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn truncate(text: &str) -> String {
    let single_line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if single_line.chars().count() <= MAX_CELL {
        single_line
    } else {
        let cut: String = single_line.chars().take(MAX_CELL - 1).collect();
        format!("{cut}…")
    }
}

fn table(columns: &[Column], rows: &[Value]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| truncate(&lookup(row, column.field).map(cell).unwrap_or_default()))
                .collect()
        })
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            cells
                .iter()
                .map(|row| row[i].chars().count())
                .chain([column.header.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |values: Vec<&str>| {
        let mut out = String::new();
        for (i, value) in values.iter().enumerate() {
            if i + 1 == values.len() {
                out.push_str(value);
            } else {
                let pad = widths[i].saturating_sub(value.chars().count());
                out.push_str(value);
                out.push_str(&" ".repeat(pad + 2));
            }
        }
        out.truncate(out.trim_end().len());
        out.push('\n');
        out
    };
    let mut out = line(columns.iter().map(|column| column.header).collect());
    for row in &cells {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

fn fields(data: &Value) -> String {
    let Value::Object(object) = data else {
        return with_newline(cell(data));
    };
    let width = object
        .keys()
        .map(|key| key.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    // Alphabetical, independent of `serde_json`'s `preserve_order` feature.
    let mut entries: Vec<_> = object.iter().collect();
    entries.sort_by_key(|(key, _)| key.as_str());
    for (key, value) in entries {
        let text = match value {
            Value::Object(_) => value.to_string(),
            other => cell(other),
        };
        out.push_str(&format!("{key:<width$}  {text}"));
        out.truncate(out.trim_end().len());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::super::find;
    use super::*;

    #[test]
    fn tables() {
        let data = json!([
            { "id": "m_b", "date": "2026-10-01T09:00:00Z", "from": "Anna <anna@example.com>", "subject": "Lunch?" },
            { "id": "m_cd", "date": "2026-10-02T10:00:00Z", "from": "Bob", "subject": "x".repeat(80) },
        ]);
        let text = render_human(find("search").unwrap(), &data, &RenderOptions::default());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[0],
            "ID    DATE                  FROM                     SUBJECT"
        );
        assert_eq!(
            lines[1],
            "m_b   2026-10-01T09:00:00Z  Anna <anna@example.com>  Lunch?"
        );
        assert!(lines[2].ends_with('…'));
        assert_eq!(lines[2].chars().count(), 53 + MAX_CELL);
    }

    #[test]
    fn ids_only() {
        let data = json!([{ "id": "m_b" }, { "id": "m_c" }]);
        let options = RenderOptions { ids_only: true };
        assert_eq!(
            render_human(find("search").unwrap(), &data, &options),
            "m_b\nm_c\n"
        );
    }

    #[test]
    fn field_and_fields() {
        let login = json!({ "export": "export MAZEMAIL_SESSION=mazemail_s_x", "id": "tk_b" });
        assert_eq!(
            render_human(find("login").unwrap(), &login, &RenderOptions::default()),
            "export MAZEMAIL_SESSION=mazemail_s_x\n"
        );
        let mutation =
            json!({ "outcome": "proposed", "proposal": "p_b", "affected": ["m_b", "m_c"] });
        assert_eq!(
            render_human(
                find("archive").unwrap(),
                &mutation,
                &RenderOptions::default()
            ),
            "affected  m_b, m_c\noutcome   proposed\nproposal  p_b\n"
        );
        // Mismatched data falls back to JSON.
        assert_eq!(
            render_human(
                find("search").unwrap(),
                &json!({ "a": 1 }),
                &RenderOptions::default()
            ),
            "{\n  \"a\": 1\n}\n"
        );
        assert_eq!(
            render_human(
                find("send").unwrap(),
                &Value::Null,
                &RenderOptions::default()
            ),
            ""
        );
    }
}

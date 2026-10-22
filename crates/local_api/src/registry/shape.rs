//! Compact, TypeScript-like renderings of JSON Schemas for AGENTS.md. Properties are
//! listed alphabetically, so the output doesn't depend on `serde_json` features.
//!
//! ```text
//! [
//!   {
//!     id: string,  // Message id.
//!     folder: string | null,
//!   }
//! ]
//! ```

use serde_json::Value;

/// How deep `$ref`s are expanded before printing just the type name.
const MAX_DEPTH: usize = 6;

/// Renders `schema` (a root schema with optional `$defs`).
pub(crate) fn shape(schema: &Value) -> String {
    render(schema, schema, 0, 0)
}

fn pad(indent: usize) -> String {
    " ".repeat(indent)
}

fn resolve<'a>(root: &'a Value, reference: &'a str) -> Option<(&'a Value, &'a str)> {
    let name = reference.strip_prefix("#/$defs/")?;
    root.get("$defs")?.get(name).map(|schema| (schema, name))
}

fn render(node: &Value, root: &Value, indent: usize, depth: usize) -> String {
    let Value::Object(map) = node else {
        return "any".to_owned();
    };
    if let Some(reference) = map.get("$ref").and_then(Value::as_str) {
        return match resolve(root, reference) {
            Some((_, name)) if depth >= MAX_DEPTH => name.to_owned(),
            Some((schema, _)) => render(schema, root, indent, depth + 1),
            None => "any".to_owned(),
        };
    }
    if let Some(value) = map.get("const") {
        return value.to_string();
    }
    if let Some(Value::Array(values)) = map.get("enum") {
        return values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(Value::Array(variants)) = map.get(key) {
            let rendered: Vec<String> = variants
                .iter()
                .map(|variant| render(variant, root, indent, depth))
                .collect();
            return union(&rendered, indent);
        }
    }
    match map.get("type") {
        Some(Value::Array(types)) => {
            let rendered: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .map(|ty| {
                    let mut single = map.clone();
                    single.insert("type".into(), Value::String(ty.to_owned()));
                    render(&Value::Object(single), root, indent, depth)
                })
                .collect();
            union(&rendered, indent)
        }
        Some(Value::String(ty)) => match ty.as_str() {
            "object" => object(map, root, indent, depth),
            "array" => {
                let items = map.get("items").map_or_else(
                    || "any".to_owned(),
                    |items| render(items, root, indent + 2, depth),
                );
                if items.contains('\n') {
                    format!("[\n{}{items}\n{}]", pad(indent + 2), pad(indent))
                } else {
                    format!("{items}[]")
                }
            }
            other => other.to_owned(),
        },
        _ => "any".to_owned(),
    }
}

fn union(variants: &[String], indent: usize) -> String {
    if variants.iter().any(|variant| variant.contains('\n')) {
        variants.join(&format!("\n{}| ", pad(indent)))
    } else {
        variants.join(" | ")
    }
}

fn object(
    map: &serde_json::Map<String, Value>,
    root: &Value,
    indent: usize,
    depth: usize,
) -> String {
    let Some(Value::Object(properties)) = map.get("properties") else {
        return match map.get("additionalProperties") {
            Some(value @ Value::Object(_)) => {
                format!(
                    "{{ [key: string]: {} }}",
                    render(value, root, indent, depth)
                )
            }
            _ => "{}".to_owned(),
        };
    };
    if properties.is_empty() {
        return "{}".to_owned();
    }
    let required: Vec<&str> = map
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut out = String::from("{\n");
    // Sorted explicitly: `serde_json` maps keep insertion order only with its
    // `preserve_order` feature, which other crates in the build may or may not enable.
    let mut properties: Vec<_> = properties.iter().collect();
    properties.sort_by_key(|(name, _)| name.as_str());
    for (name, property) in properties {
        let optional = if required.contains(&name.as_str()) {
            ""
        } else {
            "?"
        };
        let ty = render(property, root, indent + 2, depth);
        out.push_str(&format!("{}{name}{optional}: {ty},", pad(indent + 2)));
        if let Some(description) = property
            .get("description")
            .and_then(Value::as_str)
            .and_then(|text| text.lines().next())
        {
            out.push_str("  // ");
            out.push_str(description);
        }
        out.push('\n');
    }
    out.push_str(&pad(indent));
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use schemars::JsonSchema;

    use super::*;

    #[allow(dead_code)]
    #[derive(JsonSchema)]
    struct Inner {
        /// The id.
        id: String,
        count: Option<u32>,
    }

    #[allow(dead_code)]
    #[derive(JsonSchema)]
    #[serde(rename_all = "snake_case")]
    enum Kind {
        One,
        Two,
    }

    #[allow(dead_code)]
    #[derive(JsonSchema)]
    struct Outer {
        items: Vec<Inner>,
        kind: Kind,
        maybe: Option<Inner>,
        tags: Vec<String>,
    }

    #[test]
    fn renders_nested_types() {
        let schema = schemars::schema_for!(Outer).to_value();
        assert_eq!(
            shape(&schema),
            r#"{
  items: [
    {
      count?: integer | null,
      id: string,  // The id.
    }
  ],
  kind: "one" | "two",
  maybe?: {
    count?: integer | null,
    id: string,  // The id.
  }
  | null,
  tags: string[],
}"#
        );
    }
}

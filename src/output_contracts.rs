//! Tool result schemas are data, shared with wire-level conformance tests.
use serde_json::Value;
use std::sync::OnceLock;
pub fn schema(name: &str) -> Value {
    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    SCHEMAS
        .get_or_init(|| {
            serde_json::from_str(include_str!("mcp_output_schemas.json"))
                .expect("valid bundled output schemas")
        })
        .get(name)
        .unwrap_or_else(|| panic!("Missing output schema for {name}"))
        .clone()
}

/// Make a result match its advertised schema exactly: nulls are dropped and any
/// undeclared or mistyped top-level field is moved into `text` as `key: value`.
/// Strict MCP clients drop the whole result when it has fields the schema lacks.
pub fn conform(value: Value, schema: &Value) -> Value {
    let Value::Object(mut object) = without_nulls(value) else {
        return Value::Object(Default::default());
    };
    let properties = &schema["properties"];
    let mut rejected = serde_json::Map::new();
    for key in object.keys().cloned().collect::<Vec<_>>() {
        if !matches_schema(&object[&key], &properties[&key]) {
            rejected.insert(key.clone(), object.remove(&key).unwrap());
        }
    }
    if !rejected.is_empty() {
        let mut text = object
            .remove("text")
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        append_line(&mut text, &to_text(&Value::Object(rejected)));
        object.insert("text".into(), Value::String(text));
    }
    Value::Object(object)
}

fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, without_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(without_nulls).collect()),
        other => other,
    }
}

fn matches_schema(value: &Value, schema: &Value) -> bool {
    let Some(kind) = schema.get("type").and_then(Value::as_str) else {
        return false;
    };
    match (kind, value) {
        ("string", Value::String(_)) | ("boolean", Value::Bool(_)) => true,
        ("integer", Value::Number(n)) => n.is_i64() || n.is_u64(),
        ("number", Value::Number(_)) => true,
        ("array", Value::Array(items)) => items
            .iter()
            .all(|item| matches_schema(item, &schema["items"])),
        ("object", Value::Object(object)) => match schema.get("properties") {
            Some(properties) => object
                .iter()
                .all(|(k, v)| matches_schema(v, &properties[k])),
            None => true,
        },
        _ => false,
    }
}

const LEADING_KEYS: [&str; 10] = [
    "exit_code",
    "running",
    "unchanged",
    "omitted",
    "read_more",
    "read_more_note",
    "terminal_id",
    "workspace_id",
    "name",
    "status",
];

/// Plain text for `?output_type=text`: `text` verbatim, then one `key: value` line per field.
pub fn to_text(value: &Value) -> String {
    let Some(object) = value.as_object() else {
        return inline(value);
    };
    let mut out = object
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let mut keys = object
        .iter()
        .filter(|(k, v)| *k != "text" && !v.is_null())
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>();
    keys.sort_by_key(|k| {
        (
            LEADING_KEYS
                .iter()
                .position(|l| l == k)
                .unwrap_or(LEADING_KEYS.len()),
            *k,
        )
    });
    for key in keys {
        let line = match &object[key] {
            Value::Array(items) if items.is_empty() => format!("{key}: none"),
            Value::Array(items) => {
                let lines = items.iter().map(|item| format!("- {}", inline(item)));
                format!("{key}:\n{}", lines.collect::<Vec<_>>().join("\n"))
            }
            Value::String(s) if s.contains('\n') => format!("{key}:\n{}", s.trim_end_matches('\n')),
            other => format!("{key}: {}", inline(other)),
        };
        append_line(&mut out, &line);
    }
    out
}

fn append_line(out: &mut String, line: &str) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line);
}

fn inline(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Object(object) => object
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| format!("{k}: {}", inline(v)))
            .collect::<Vec<_>>()
            .join(", "),
        Value::Array(items) => items.iter().map(inline).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn conform_moves_undeclared_fields_into_text() {
        let schema = schema("webterm");
        let value = conform(
            json!({"text":"up 10 days\n","exit_code":0,"running":false,"chars":11,
                   "terminal_id":681,"workspace_id":"/tmp","snapshot":"ce9977bd8cc06314",
                   "mystery":"x","status":null,"total":"bad"}),
            &schema,
        );
        assert_eq!(value["text"], "up 10 days\nmystery: x\ntotal: bad");
        assert!(value.get("status").is_none());
        for key in [
            "exit_code",
            "running",
            "chars",
            "terminal_id",
            "workspace_id",
            "snapshot",
        ] {
            assert!(value.get(key).is_some(), "{key}");
        }
        for key in value.as_object().unwrap().keys() {
            assert!(schema["properties"].get(key).is_some(), "{key}");
        }
    }

    #[test]
    fn plain_text_puts_output_first_then_one_field_per_line() {
        let text = to_text(
            &json!({"text":" 01:48:46 up 10 days\n","exit_code":1,"running":false,
            "chars":21,"terminal_id":681,"workspace_id":"/tmp","snapshot":"ce9977bd8cc06314"}),
        );
        assert_eq!(
            text,
            " 01:48:46 up 10 days\nexit_code: 1\nrunning: false\nterminal_id: 681\nworkspace_id: /tmp\nchars: 21\nsnapshot: ce9977bd8cc06314"
        );
        let listed = to_text(
            &json!({"total":1,"terminals":[{"terminal_id":3,"name":"a","status":"running"}],"notes":[]}),
        );
        assert_eq!(
            listed,
            "notes: none\nterminals:\n- name: a, status: running, terminal_id: 3\ntotal: 1"
        );
        assert_eq!(
            to_text(&json!({"text":"no newline","exit_code":0})),
            "no newline\nexit_code: 0"
        );
    }
}

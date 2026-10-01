//! One bounded text preview for all compact MCP command results.
use serde_json::{Value, json};
use std::io::Read;

pub const DEFAULT_CHARS: usize = 1000;

/// Character-safe first 20% + last 80%; at the default budget this is 200/800.
pub fn shorten(text: &str, budget: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= budget {
        return text.to_owned();
    }
    let head = budget / 5;
    chars[..head]
        .iter()
        .chain(chars[chars.len() - (budget - head)..].iter())
        .collect()
}

/// Drain stderr without pipe deadlock or unbounded memory, including helper crashes.
pub fn drain_stderr(mut reader: impl Read) -> std::io::Result<String> {
    let mut head = Vec::new();
    let mut tail = std::collections::VecDeque::new();
    let mut total = 0usize;
    let mut buf = [0u8; 4096];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n);
        let take = n.min(4096usize.saturating_sub(head.len()));
        head.extend_from_slice(&buf[..take]);
        tail.extend(&buf[..n]);
        while tail.len() > 12288 {
            tail.pop_front();
        }
    }
    let bytes = if total <= 12288 {
        tail.into_iter().collect::<Vec<u8>>()
    } else {
        head.truncate(total.saturating_sub(12288).min(4096));
        head.extend(tail);
        head
    };
    Ok(shorten(&String::from_utf8_lossy(&bytes), DEFAULT_CHARS))
}

/// Put diagnostics into the actual text, not in a second, easily missed field.
/// Successful writes remain compact receipts and never echo submitted input.
pub fn compact(mut value: Value, budget: usize) -> Value {
    let Some(object) = value.as_object_mut() else {
        return value;
    };
    let base = object.remove("output").or_else(|| object.remove("text"));
    let mut text = base.and_then(|v| v.as_str().map(str::to_owned));
    let diagnostics = ["stderr", "filter_stderr"]
        .into_iter()
        .filter_map(|key| {
            object
                .remove(key)
                .and_then(|v| v.as_str().map(str::to_owned))
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
    if text.is_none() && diagnostics.is_empty() {
        return value;
    }
    let mut text = text.take().unwrap_or_default();
    let original_chars = text.chars().count() as u64;
    let mut total = object
        .get("chars")
        .or_else(|| object.get("output_chars"))
        .and_then(Value::as_u64)
        .unwrap_or(original_chars)
        .max(original_chars);
    let mut included = Vec::new();
    for diagnostic in diagnostics {
        if included.contains(&diagnostic) {
            continue;
        }
        if !shorten(&text, budget).contains(&diagnostic) {
            let extra = format!("\n[stderr]\n{diagnostic}");
            total = total.saturating_add(extra.chars().count() as u64);
            text.push_str(&extra);
        }
        included.push(diagnostic);
    }
    let shown = shorten(&text, budget);
    let shown_chars = shown.chars().count() as u64;
    object.insert("text".into(), json!(shown));
    if object.contains_key("chars") || total > shown_chars || !included.is_empty() {
        object.insert("chars".into(), json!(total));
    }
    let omitted = total.saturating_sub(shown_chars);
    if omitted > 0 {
        object.insert("omitted".into(), json!(omitted));
        let terminal = object.get("terminal_id").and_then(Value::as_i64);
        let source = object.get("source_terminal_id").and_then(Value::as_i64);
        if let Some(id) = terminal.or(source).filter(|id| *id > 0) {
            object.insert(
                "read_more".into(),
                json!(format!("webterm read {id} --full")),
            );
            if terminal.is_none() || object.contains_key("filter_exit_code") {
                object.insert(
                    "read_more_note".into(),
                    json!("Reads original terminal output, before the pipeline/filter."),
                );
            }
        } else {
            object.insert("read_more".into(), json!("This control output has no terminal ID. For long commands use webterm run, then webterm read ID --full."));
        }
        if object.get("retention_limited") == Some(&json!(true)) {
            object.insert("read_more_note".into(), json!("Only retained output can be read; the older middle exceeded the retention limit."));
        }
    } else {
        object.remove("omitted");
        object.remove("read_more");
        object.remove("read_more_note");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_unicode_boundaries() {
        for n in [0, 1, 999, 1000, 1001, 8000] {
            let s = "界".repeat(n);
            assert_eq!(shorten(&s, 1000).chars().count(), n.min(1000));
        }
        assert_eq!(
            shorten(
                &format!("{}MIDDLE{}", "🙂".repeat(200), "界".repeat(800)),
                1000
            ),
            format!("{}{}", "🙂".repeat(200), "界".repeat(800))
        );
        assert_eq!(shorten("abc", 0), "");
    }
    #[test]
    fn preview_and_real_read_more_id() {
        let source = format!("{}MIDDLE{}", "h".repeat(200), "t".repeat(800));
        let v = compact(json!({"output":source,"terminal_id":41,"chars":1006}), 1000);
        assert_eq!(v["text"], format!("{}{}", "h".repeat(200), "t".repeat(800)));
        assert_eq!(v["omitted"], 6);
        assert_eq!(v["read_more"], "webterm read 41 --full");
        assert!(v.get("output").is_none());
    }
    #[test]
    fn stderr_survives_noisy_stdout_and_is_not_duplicated() {
        let v = compact(
            json!({"output":"x".repeat(5000),"stderr":"Important failure","exit_code":2,"terminal_id":7}),
            1000,
        );
        assert_eq!(v["text"].as_str().unwrap().chars().count(), 1000);
        assert!(
            v["text"]
                .as_str()
                .unwrap()
                .ends_with("[stderr]\nImportant failure")
        );
        assert!(v.get("stderr").is_none());
        let v = compact(
            json!({"output":"failure\n","stderr":"failure\n","exit_code":1}),
            1000,
        );
        assert_eq!(v["text"], "failure\n");
    }
    #[test]
    fn stderr_only_filter_and_successful_write() {
        let v = compact(
            json!({"filter_stderr":"bad filter","filter_exit_code":2}),
            1000,
        );
        assert!(v["text"].as_str().unwrap().contains("bad filter"));
        let receipt = json!({"terminal_id":3,"bytes_written":65000,"enter":true});
        assert_eq!(compact(receipt.clone(), 1000), receipt);
    }
    #[test]
    fn explicit_read_can_expand_without_rerunning() {
        let v = compact(json!({"output":"a".repeat(5000),"terminal_id":8}), 262144);
        assert_eq!(v["text"].as_str().unwrap().len(), 5000);
        assert!(v.get("read_more").is_none());
    }
    #[test]
    fn helper_stderr_is_drained_and_bounded() {
        let input = format!("{}MIDDLE{}", "h".repeat(200), "z".repeat(20000));
        let v = drain_stderr(input.as_bytes()).unwrap();
        assert_eq!(v, format!("{}{}", "h".repeat(200), "z".repeat(800)));
    }
}

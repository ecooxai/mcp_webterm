//! Compact, non-shell command grammar shared by MCP, audit attribution and CLI.
//! Only explicit run/python payloads and --filter are executed as programs.
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub const MAX_CMD_BYTES: usize = 96 * 1024;
pub const DESCRIPTION: &str = "Bash and WebTerm controls. Read/write accept workspace (absolute folder), task (simple name), summary (n/100 then current progress, fewer than 50 words total). Prefer cmd='webterm run /path', text='code' or cmd='webterm write ID --enter', text='input'. text is literal; no outer-shell quoting. Without text, cmd supports Bash pipes, e.g. webterm read ID | grep error. ls terminals lists native IDs. Long work returns terminal_id; read it, never rerun.";
pub const INSTRUCTIONS: &str = "Use webterm(cmd) for Bash and controls; optional text is literal run/python code or write input. On read/write include workspace (absolute existing folder), task (simple name), summary (n/100 then progress, fewer than 50 words total). Metadata works with pipelines: webterm read ID | grep error. ls terminals lists native IDs. Controls use no PTY slots. Poll returned terminal_id; never rerun. get_image returns native images. Private previews: https://PORT-proxy-colabdev.alima.freeddns.org/.";

#[derive(Debug, Clone)]
pub struct Parsed {
    pub op: String,
    pub json: bool,
    pub args: Map<String, Value>,
    pub max_chars: usize,
    pub full: bool,
    pub if_changed: Option<String>,
    pub wait: f64,
    pub limit: usize,
    pub offset: usize,
    pub task: Option<String>,
    pub summary: Option<String>,
}

// Tokenize the header only. Never expand variables, globs, substitutions or escapes
// in a raw payload after an unquoted --. This preserves multiline scripts exactly.
fn lex(src: &str) -> Result<(Vec<String>, Option<String>)> {
    if src.len() > MAX_CMD_BYTES || src.contains('\0') {
        bail!("cmd must be at most 98304 UTF-8 bytes, without NUL");
    }
    let mut iter = src.char_indices().peekable();
    let mut words = Vec::new();
    while let Some(&(start, ch)) = iter.peek() {
        if ch.is_whitespace() {
            iter.next();
            continue;
        }
        let mut word = String::new();
        let mut quote = None;
        let mut quoted = false;
        let mut end = start;
        while let Some(&(idx, c)) = iter.peek() {
            if quote.is_none() && c.is_whitespace() {
                break;
            }
            iter.next();
            end = idx + c.len_utf8();
            match (quote, c) {
                (None, '\'' | '"') => {
                    quote = Some(c);
                    quoted = true;
                }
                (Some(q), c) if q == c => quote = None,
                (None, '\\') => {
                    let (i, c) = iter.next().context("trailing backslash in cmd header")?;
                    word.push(c);
                    end = i + c.len_utf8();
                    quoted = true;
                }
                (Some('"'), '\\') => {
                    if let Some(&(_, c)) = iter.peek() {
                        if matches!(c, '"' | '\\') {
                            let (i, c) = iter.next().unwrap();
                            word.push(c);
                            end = i + c.len_utf8();
                        } else {
                            word.push('\\');
                        }
                    } else {
                        bail!("unclosed double quote");
                    }
                }
                _ => word.push(c),
            }
        }
        if quote.is_some() {
            bail!("unclosed quote in cmd header");
        }
        if word == "--" && !quoted {
            let rest = &src[end..];
            // Remove just the delimiter separator, not intentional payload whitespace.
            let payload = rest
                .strip_prefix(' ')
                .or_else(|| rest.strip_prefix('\n'))
                .or_else(|| rest.strip_prefix('\t'))
                .unwrap_or(rest);
            return Ok((words, Some(payload.to_owned())));
        }
        words.push(word);
    }
    Ok((words, None))
}

pub fn parse(src: &str) -> Result<Parsed> {
    let (mut words, payload) = lex(src)?;
    if words.first().map(String::as_str) == Some("webterm") {
        words.remove(0);
    }
    let first = words.first().context("missing command; use help")?;
    let op = match first.as_str() {
        "list" => "ls",
        "capture" => "read",
        "bash" | "exec" => "run",
        "create" => "new",
        other => other,
    }
    .to_owned();
    if ![
        "help", "status", "ls", "ensure", "new", "read", "write", "run", "python", "resize", "stop",
    ]
    .contains(&op.as_str())
    {
        bail!("unknown command {op:?}; use help");
    }
    let mut result = Parsed {
        op,
        json: false,
        args: Map::new(),
        max_chars: 2000,
        full: false,
        if_changed: None,
        wait: 0.0,
        limit: 50,
        offset: 0,
        task: None,
        summary: None,
    };
    let mut positional = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        if !word.starts_with("--") {
            positional.push(word.clone());
            i += 1;
            continue;
        }
        let (flag, inline) = word
            .split_once('=')
            .map_or((word.as_str(), None), |(a, b)| (a, Some(b)));
        if !seen.insert(flag.to_owned()) {
            bail!("duplicate option {flag}");
        }
        let allowed = match flag {
            "--task" | "--summary" | "--json" => true,
            "--name" | "--cols" | "--rows" => result.op == "new",
            "--enter" => result.op == "write",
            "--lines" | "--filter" | "--if-changed" => result.op == "read",
            "--full" | "--max-chars" | "--wait" => {
                ["read", "run", "python"].contains(&result.op.as_str())
            }
            "--limit" | "--offset" => result.op == "ls",
            _ => false,
        };
        if !allowed {
            bail!(
                "option {flag} is not supported by {}; use help {}",
                result.op,
                result.op
            );
        }
        if flag == "--full" || flag == "--enter" || flag == "--json" {
            if inline.is_some() {
                bail!("{flag} does not take a value");
            }
            if flag == "--json" {
                result.json = true;
            } else if flag == "--full" {
                result.full = true;
            } else {
                result.args.insert("enter".into(), json!(true));
            }
            i += 1;
            continue;
        }
        let value = match inline {
            Some(v) => v.to_owned(),
            None => {
                i += 1;
                words
                    .get(i)
                    .context(format!("missing value for {flag}"))?
                    .clone()
            }
        };
        match flag {
            "--task" => result.task = Some(value),
            "--summary" => result.summary = Some(value),
            "--name" => {
                result.args.insert("name".into(), json!(value));
            }
            "--filter" => {
                if value.trim().is_empty() || value.len() > 4096 {
                    bail!("--filter must contain 1..4096 UTF-8 bytes");
                }
                result.args.insert("filter_cmd".into(), json!(value));
            }
            "--if-changed" => {
                if value.len() != 16 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
                    bail!("--if-changed requires the 16-character snapshot returned by read/run");
                }
                result.if_changed = Some(value);
            }
            "--wait" => {
                result.wait = value
                    .parse()
                    .context("--wait must be a number from 0 to 20")?;
                if !result.wait.is_finite() || !(0.0..=20.0).contains(&result.wait) {
                    bail!("--wait must be from 0 to 20");
                }
            }
            "--max-chars" => result.max_chars = number(&value, 1, 262144, flag)?,
            "--limit" => result.limit = number(&value, 1, 200, flag)?,
            "--offset" => result.offset = number(&value, 0, 1_000_000, flag)?,
            "--lines" | "--cols" | "--rows" => {
                let (min, max) = match flag {
                    "--lines" => (1, 1000),
                    "--cols" => (2, crate::runtime::MAX_COLS as usize),
                    _ => (2, crate::runtime::MAX_ROWS as usize),
                };
                result
                    .args
                    .insert(flag[2..].to_owned(), json!(number(&value, min, max, flag)?));
            }
            _ => unreachable!(),
        }
        i += 1;
    }
    if result.task.is_some() != result.summary.is_some() {
        bail!("--task and --summary must be supplied together");
    }
    if result.full && seen.contains("--max-chars") {
        bail!("choose --full or --max-chars, not both");
    }
    if result.full {
        result.max_chars = 262144;
    }
    if result.wait > 0.0 && result.args.contains_key("filter_cmd") {
        bail!("--filter cannot be combined with --wait; filters execute only once");
    }
    if result.op == "ls"
        && positional
            .first()
            .is_some_and(|s| s == "terminals" || s == "workspaces")
    {
        let kind = positional.remove(0);
        result.args.insert("kind".into(), json!(kind));
    }
    let inferred = matches!(result.op.as_str(), "read" | "write" | "stop") && positional.len() == 1
        || result.op == "resize" && positional.len() == 3;
    if inferred {
        positional.insert(0, "/".to_owned());
    }
    let expected = match result.op.as_str() {
        "help" | "ls" => 0..=1,
        "status" => 0..=0,
        "new" => 1..=2,
        "ensure" | "run" | "python" => 1..=1,
        "resize" => 4..=4,
        _ => 2..=2,
    };
    if !expected.contains(&positional.len()) {
        bail!("wrong arguments for {}; use help {}", result.op, result.op);
    }
    if result.op == "help" {
        if let Some(topic) = positional.first() {
            result.args.insert("topic".into(), json!(topic));
        }
    } else if let Some(path) = positional.first() {
        if !path.starts_with('/') || path.len() > 4096 {
            bail!("workspace_id must be an absolute folder path of at most 4096 bytes");
        }
        result.args.insert("workspace_id".into(), json!(path));
    }
    match result.op.as_str() {
        "new" if positional.len() == 2 => {
            if result.args.contains_key("name") {
                bail!("name supplied twice");
            }
            result.args.insert("name".into(), json!(positional[1]));
        }
        "read" | "write" | "resize" | "stop" => {
            let id = &positional[1];
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
                bail!("terminal_id must be a positive numeric ID returned by new/run/ls");
            }
            let id: i64 = id.parse().context("terminal_id is too large")?;
            if id < 1 {
                bail!("terminal_id must be positive");
            }
            result.args.insert("terminal_id".into(), json!(id));
            if result.op == "resize" {
                result.args.insert(
                    "cols".into(),
                    json!(number(
                        &positional[2],
                        2,
                        crate::runtime::MAX_COLS as usize,
                        "cols"
                    )?),
                );
                result.args.insert(
                    "rows".into(),
                    json!(number(
                        &positional[3],
                        2,
                        crate::runtime::MAX_ROWS as usize,
                        "rows"
                    )?),
                );
            }
        }
        _ => {}
    }
    match result.op.as_str() {
        "write" | "run" | "python" => {
            let data = payload.context("missing -- delimiter and literal payload; use help")?;
            let maximum = if result.op == "write" { 65536 } else { 32768 };
            if data.len() > maximum || (result.op != "write" && data.trim().is_empty()) {
                bail!(
                    "invalid payload: maximum {maximum} UTF-8 bytes; run/python must be nonempty"
                );
            }
            let key = match result.op.as_str() {
                "write" => "data",
                "python" => "code",
                _ => "command",
            };
            result.args.insert(key.into(), json!(data));
            if result.op != "write" {
                result.args.insert(
                    "wait_s".into(),
                    json!(if seen.contains("--wait") {
                        result.wait
                    } else {
                        20.0
                    }),
                );
            }
        }
        _ if payload.is_some() => bail!("{} does not accept a -- payload", result.op),
        _ => {}
    }
    if ["read", "run", "python"].contains(&result.op.as_str()) {
        result.args.insert("full_output".into(), json!(true));
    }
    if inferred {
        result.args.remove("workspace_id");
    }
    Ok(result)
}

/// Attribution only: recognize an explicitly quoted compatibility command.
/// This parser never executes shell code or chooses execution permissions.
pub fn parse_for_tracking(text: &str) -> Result<Parsed> {
    let (words, payload) = lex(text)?;
    if payload.is_none() && words.len() == 3 && words[0] == "webterm" && words[1] == "cmd" {
        return parse(&words[2]);
    }
    parse(text)
}

/// A separate literal payload is never evaluated by the outer shell.
pub fn with_text(cmd: &str, text: &str) -> Result<String> {
    let (mut words, inline) = lex(cmd)?;
    if words.first().map(String::as_str) == Some("webterm") {
        words.remove(0);
    }
    if !matches!(
        words.first().map(String::as_str),
        Some("run" | "bash" | "exec" | "python" | "write")
    ) {
        bail!("text is supported only with one run, python or write command");
    }
    if inline.as_ref().is_some_and(|value| !value.is_empty()) {
        bail!("put the payload in text or after --, not both");
    }
    if cmd.len() > 8192 {
        bail!("cmd header with text must be at most 8192 UTF-8 bytes");
    }
    let normalized = format!("{} -- {}", from_argv(&words), text);
    parse(&normalized)?;
    Ok(normalized)
}

fn number(value: &str, min: usize, max: usize, name: &str) -> Result<usize> {
    let n: usize = value
        .parse()
        .with_context(|| format!("{name} must be an integer from {min} to {max}"))?;
    if !(min..=max).contains(&n) {
        bail!("{name} must be from {min} to {max}");
    }
    Ok(n)
}

/// Reconstruct the header with quoting; raw argv after -- is joined as the shell
/// passed it. Quote a whole script argument at a shell prompt to preserve syntax.
pub fn from_argv(args: &[String]) -> String {
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let header = args[..split]
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    if split < args.len() {
        format!("{header} -- {}", args[split + 1..].join(" "))
    } else {
        header
    }
}

pub fn help(topic: Option<&str>) -> Result<Value> {
    let rows = [
        (
            "new",
            "new WORKSPACE [NAME] [--name NAME] [--cols 80] [--rows 24]",
        ),
        (
            "read",
            "read [WORKSPACE] ID [--json] [--lines 100] [--max-chars 2000|--full] [--if-changed SNAPSHOT] [--wait 0..20] [--filter 'BASH']",
        ),
        ("write", "write WORKSPACE ID [--enter] -- LITERAL_INPUT"),
        (
            "run",
            "run WORKSPACE [--wait 20] [--max-chars 2000|--full] -- BASH_CODE",
        ),
        (
            "python",
            "python WORKSPACE [--wait 20] [--max-chars 2000|--full] -- PYTHON_CODE",
        ),
        (
            "ls",
            "ls [workspaces|terminals] [WORKSPACE] [--limit 50] [--offset 0] [--json]",
        ),
        ("ensure", "ensure WORKSPACE"),
        ("resize", "resize WORKSPACE ID COLS ROWS"),
        ("stop", "stop WORKSPACE ID"),
        ("status", "status"),
    ];
    let commands = rows
        .iter()
        .filter(|(op, _)| topic.is_none() || topic == Some(*op))
        .map(|(_, usage)| json!(usage))
        .collect::<Vec<_>>();
    if commands.is_empty() {
        bail!("unknown help topic; use help");
    }
    Ok(json!({"commands":commands,"notes":[
        "MCP accepts optional text for one run, python or write header. Example: cmd=webterm run /path, text=code. Text is literal, not outer-shell-expanded; omit inline code and pipelines when text is supplied.",
        "Optional leading webterm. WORKSPACE is an absolute folder path; quote paths with spaces. ID is the numeric terminal_id returned by new/run/ls; zero padding is accepted, and workspace ownership is checked.",
        "All terminal tools are consolidated here. get_image stays separate for native image content. Legacy tools remain callable but are not advertised.",
        "The MCP command is Bash. Quote scripts and write payloads after -- to prevent evaluation by the outer shell. CLI read prints retained text for pipes; --json requests structured metadata. webterm cmd accepts the old literal grammar as one quoted argument.",
        "run/python create one tracked command. When running=true, use read, never resubmit. Default output is first 500 plus last 1500 characters. omitted reports missing characters; --full still respects 256 KiB retention.",
        "read returns a snapshot fingerprint. Reuse it with --if-changed to omit unchanged output; add --wait 20 to long-poll. This is a non-cryptographic change detector, not a resumable log cursor.",
        "--filter runs Bash once on the retained snapshot via stdin, not in the target PTY. It has shell permissions, a five-second limit, and cannot combine with --wait.",
        "Optional --task NAME --summary '35/100 Brief action' adds progress attribution. Authentication and workspace root restrictions are unchanged."
    ]}))
}

pub fn compact_output(raw: &Value, cmd: &Parsed) -> Value {
    let text = raw["output"].as_str().unwrap_or("");
    // Deterministic FNV-1a fingerprint: no server-side cursor state or destructive reads.
    let mut hash = 0xcbf29ce484222325u64;
    let state = json!([
        text,
        raw["running"],
        raw["exit_code"],
        raw["output_chars"],
        raw["retention_limited"].as_bool().unwrap_or(false),
        raw["capture_limited"].as_bool().unwrap_or(false),
        raw["interrupted"].as_bool().unwrap_or(false),
        raw["filter_exit_code"],
        raw["filter_stderr"],
        raw["filter_timed_out"].as_bool().unwrap_or(false),
        raw["filter_output_limit_hit"].as_bool().unwrap_or(false)
    ]);
    for byte in state.to_string().bytes().chain(cmd.max_chars.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let snapshot = format!("{hash:016x}");
    let mut value = json!({"workspace_id":raw["workspace_id"],"terminal_id":raw["terminal_id"],"running":raw["running"],"exit_code":raw["exit_code"],"snapshot":snapshot});
    if cmd.if_changed.as_deref() == Some(snapshot.as_str()) {
        value["unchanged"] = json!(true);
        return value;
    }
    let chars = text.chars().collect::<Vec<_>>();
    let total = raw["output_chars"]
        .as_u64()
        .unwrap_or(chars.len() as u64)
        .max(chars.len() as u64);
    let shown = if chars.len() > cmd.max_chars {
        let head = cmd.max_chars / 4;
        chars[..head]
            .iter()
            .chain(chars[chars.len() - (cmd.max_chars - head)..].iter())
            .collect::<String>()
    } else {
        text.to_owned()
    };
    let omitted = total.saturating_sub(shown.chars().count() as u64);
    value["output"] = json!(shown);
    value["chars"] = json!(total);
    if omitted > 0 {
        value["omitted"] = json!(omitted);
    }
    for key in [
        "retention_limited",
        "capture_limited",
        "interrupted",
        "filter_exit_code",
        "filter_stderr",
        "filter_input_limited",
        "filter_stderr_truncated",
        "filter_timed_out",
        "filter_output_limit_hit",
    ] {
        if let Some(v) = raw.get(key)
            && !v.is_null()
            && v != &json!(false)
            && v != &json!("")
        {
            value[key] = v.clone();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_multiline_payload() {
        let p = parse("webterm run '/tmp/a b' -- printf '%s\\n' \"$HOME\"\n# keep\n  echo done\n")
            .unwrap();
        assert_eq!(p.args["workspace_id"], "/tmp/a b");
        assert_eq!(
            p.args["command"],
            "printf '%s\\n' \"$HOME\"\n# keep\n  echo done\n"
        );
        assert_eq!(p.args["wait_s"], 20.0);
    }
    #[test]
    fn write_preserves_input_and_enter_is_explicit() {
        let p = parse("write /tmp 01 --   --full\n").unwrap();
        assert_eq!(p.args["data"], "  --full\n");
        assert!(!p.args.contains_key("enter"));
        assert_eq!(p.args["terminal_id"], 1);
        assert_eq!(parse("write /tmp 1 --enter -- ").unwrap().args["data"], "");
    }
    #[test]
    fn invalid_commands_and_options() {
        for cmd in [
            "",
            "webterm",
            "nope",
            "read /tmp",
            "new relative",
            "read /tmp 0",
            "read /tmp -1",
            "read /tmp 999999999999999999999999999",
            "read /tmp 1 --lines 0",
            "new /tmp --cols 99999",
            "new /tmp x --name y",
            "run /tmp --wait NaN -- true",
            "run /tmp --wait 21 -- true",
            "run /tmp",
            "read /tmp 1 --full --max-chars 5",
            "read /tmp 1 --full --full",
            "write /tmp 1 --full -- x",
            "read /tmp 1 --if-changed invalid",
            "read /tmp 1 --wait 1 --filter cat",
            "status extra",
            "status -- echo hi",
            "help nonexistent",
            "new '/unclosed",
            "new /tmp --task task",
        ] {
            let p = parse(cmd);
            if cmd == "help nonexistent" {
                assert!(help(Some("nonexistent")).is_err());
            } else {
                assert!(p.is_err(), "{cmd}");
            }
        }
    }
    #[test]
    fn every_command_and_alias() {
        for c in [
            "status",
            "help",
            "help write",
            "ls",
            "ls /tmp --limit=1 --offset=0",
            "ensure /tmp",
            "new /tmp name",
            "resize /tmp 1 80 24",
            "stop /tmp 1",
            "python /tmp -- print(1)",
            "capture /tmp 1",
            "list /tmp",
            "bash /tmp -- echo x",
            "create /tmp",
        ] {
            assert!(parse(c).is_ok(), "{c}");
        }
    }
    #[test]
    fn byte_limits_and_unicode() {
        assert!(parse(&format!("write /tmp 1 -- {}", "界".repeat(21846))).is_err());
        assert!(parse("new \"/tmp/界 \\\"x\\\"\"").is_ok());
        assert!(parse("status\0").is_err());
    }
    #[test]
    fn argv_roundtrip() {
        let args = vec!["run", "/tmp/a'b c", "--", "printf '%s\\n' \"hello\""]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let p = parse(&from_argv(&args)).unwrap();
        assert_eq!(p.args["workspace_id"], args[1]);
        assert_eq!(p.args["command"], args[3]);
    }
    #[test]
    fn compact_unicode_preview_and_unchanged() {
        let mut cmd = parse("read /tmp 1 --max-chars 4").unwrap();
        let raw = json!({"workspace_id":"/tmp","terminal_id":1,"running":false,"exit_code":0,"output":"界abcdef🙂","output_chars":8});
        let first = compact_output(&raw, &cmd);
        assert_eq!(first["output"], "界ef🙂");
        assert_eq!(first["omitted"], 4);
        cmd.if_changed = Some(first["snapshot"].as_str().unwrap().into());
        let same = compact_output(&raw, &cmd);
        assert_eq!(same["unchanged"], true);
        assert!(same.get("output").is_none());
        let mut changed = raw.clone();
        changed["running"] = json!(true);
        assert!(compact_output(&changed, &cmd).get("unchanged").is_none());
        cmd.max_chars = 10;
        assert!(compact_output(&raw, &cmd).get("unchanged").is_none());
    }

    #[test]
    fn snapshot_ignores_transient_runner_metadata_but_catches_middle_changes() {
        let cmd = parse("read /tmp 1").unwrap();
        let raw = json!({"workspace_id":"/tmp","terminal_id":1,"running":true,"exit_code":null,"output":"x","output_chars":1,"elapsed_s":1.1,"updated_at":1});
        let first = compact_output(&raw, &cmd);
        let mut second = raw.clone();
        second["elapsed_s"] = json!(9.0);
        second["updated_at"] = json!(10);
        assert_eq!(first["snapshot"], compact_output(&second, &cmd)["snapshot"]);
        second["output"] = json!("y");
        assert_ne!(first["snapshot"], compact_output(&second, &cmd)["snapshot"]);
    }
    #[test]
    fn arbitrary_unicode_headers_do_not_panic() {
        let alphabet = ['a', '/', ' ', '\'', '"', '\\', '界', '\n', '-', '=', '\t'];
        let mut seed = 123456u64;
        for _ in 0..5000 {
            let mut s = String::new();
            for _ in 0..40 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                s.push(alphabet[(seed as usize) % alphabet.len()]);
            }
            let _ = parse(&s);
        }
    }
}

#[cfg(test)]
mod text_parameter_tests {
    use super::*;
    #[test]
    fn literal_text_preserves_multiline_quotes_unicode_and_whitespace() {
        let text = "  printf '%s\\n' \"$HOME\"\n# café 界🙂\n";
        let normalized = with_text("webterm run '/tmp/space name' --wait 0", text).unwrap();
        let parsed = parse(&normalized).unwrap();
        assert_eq!(parsed.args["command"], text);
        assert_eq!(parsed.args["workspace_id"], "/tmp/space name");
        assert_eq!(parsed.args["wait_s"], 0.0);
    }
    #[test]
    fn literal_write_including_empty_input_is_exact() {
        for text in [
            "",
            "  spaces\n",
            "$(printf untouched); `literal` | > & ' \\",
            "\u{3}",
        ] {
            let parsed = parse(&with_text("write 123 --enter", text).unwrap()).unwrap();
            assert_eq!(parsed.args["data"], text);
            assert_eq!(parsed.args["enter"], true);
            assert_eq!(parsed.args["terminal_id"], 123);
        }
    }
    #[test]
    fn rejects_conflicts_and_unsupported_headers() {
        for (cmd, text) in [
            ("read 1", "x"),
            ("status", "x"),
            ("run /tmp -- old", "new"),
            ("run /tmp | cat", "x"),
            ("write 1; echo invalid", "x"),
            ("run /tmp", ""),
            ("python /tmp", " \n"),
            ("write 1", "\0"),
        ] {
            assert!(with_text(cmd, text).is_err(), "{cmd}");
        }
        assert!(with_text("run /tmp --", "true").is_ok());
        assert!(with_text("write 1 --", "").is_ok());
        assert!(with_text("python /tmp", "print('hi')").is_ok());
    }
    #[test]
    fn payload_byte_limits_apply_without_shell_quote_expansion() {
        assert!(with_text("write 1", &"'".repeat(65536)).is_ok());
        assert!(with_text("write 1", &"x".repeat(65537)).is_err());
        assert!(with_text("run /tmp", &"#".repeat(32768)).is_ok());
        assert!(with_text("run /tmp", &"x".repeat(32769)).is_err());
        assert!(with_text("write 1", &"界".repeat(21846)).is_err());
    }
}

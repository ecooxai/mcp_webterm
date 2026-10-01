//! Private, bounded, durable MCP invocation history. Never stores authentication headers or image payloads.
use crate::config::Config;
use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
const KEEP: i64 = 2000;
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
fn db(config: &Config) -> Result<Connection> {
    config.ensure_state_dirs()?;
    let path = config.database_path.with_extension("mcp-log.db");
    let db = Connection::open(&path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    db.busy_timeout(Duration::from_secs(3))?;
    db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS calls(id INTEGER PRIMARY KEY AUTOINCREMENT,started_ms INTEGER NOT NULL,finished_ms INTEGER,status TEXT NOT NULL,tool TEXT NOT NULL,task TEXT NOT NULL,summary TEXT NOT NULL,workspace TEXT NOT NULL,duration_ms INTEGER,input_size INTEGER NOT NULL,output_size INTEGER NOT NULL DEFAULT 0,arguments TEXT NOT NULL,output TEXT); CREATE INDEX IF NOT EXISTS calls_workspace ON calls(workspace,id); CREATE INDEX IF NOT EXISTS calls_status ON calls(status,id);")?;
    Ok(db)
}
pub fn initialize(config: &Config) {
    if let Ok(db) = db(config) {
        let _ = db.execute(
            "UPDATE calls SET status='interrupted',finished_ms=?1 WHERE status='running'",
            [now()],
        );
    }
}
fn cut(s: &str, n: usize) -> String {
    let mut value = s.chars().take(n).collect::<String>();
    if s.chars().count() > n {
        value.push_str("… [truncated]");
    }
    value
}
pub fn sanitize(config: &Config, v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let lower = k.to_ascii_lowercase();
                    let hidden = [
                        "password",
                        "passwd",
                        "token",
                        "authorization",
                        "cookie",
                        "secret",
                        "api_key",
                        "private_key",
                    ]
                    .iter()
                    .any(|s| lower.contains(s))
                        || (k == "data" && m.get("type").and_then(Value::as_str) == Some("image"));
                    (
                        k.clone(),
                        if hidden {
                            json!("[redacted]")
                        } else {
                            sanitize(config, v)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().take(200).map(|v| sanitize(config, v)).collect()),
        Value::String(s) => {
            let mut s = s.clone();
            for secret in [&config.auth_token, &config.web_password]
                .into_iter()
                .flatten()
            {
                if !secret.is_empty() {
                    s = s.replace(secret, "[redacted]");
                }
            }
            if s.contains("-----BEGIN") && s.contains("PRIVATE KEY") {
                return json!("[private key redacted]");
            }
            json!(cut(&s, 8000))
        }
        _ => v.clone(),
    }
}
fn encoded(config: &Config, v: &Value) -> String {
    let clean = sanitize(config, v);
    let s = clean.to_string();
    if s.len() > 24000 {
        json!({"preview":cut(&s,16000),"truncated":true}).to_string()
    } else {
        s
    }
}
pub fn begin(config: &Config, params: Option<&serde_json::Map<String, Value>>) -> Result<i64> {
    let p = params.cloned().unwrap_or_default();
    let args = p.get("arguments").cloned().unwrap_or(json!({}));
    let original_args = args.clone();
    // Derive attribution separately; never inject inferred fields into logged inputs.
    // Derive attribution from the same validated grammar without executing payloads.
    let mut args = args;
    if p.get("name").and_then(Value::as_str) == Some("webterm") {
        if let Some(workspace) = args.get("workspace").cloned() {
            args["workspace_id"] = workspace;
        }
        if let Some(cmd) = args.get("cmd").and_then(Value::as_str).and_then(|s| {
            if let Some(text) = args.get("text").and_then(Value::as_str) {
                crate::webterm_cmd::with_text(s, text)
                    .ok()
                    .and_then(|value| crate::webterm_cmd::parse(&value).ok())
            } else {
                crate::webterm_cmd::parse_for_tracking(s).ok()
            }
        }) {
            if args.get("workspace_id").is_none() {
                if let Some(workspace) = cmd.args.get("workspace_id") {
                    args["workspace_id"] = workspace.clone();
                }
            }
            if args.get("task").is_none() {
                args["task"] = json!(cmd.task.unwrap_or_else(|| "Untracked command".into()));
            }
            if args.get("summary").is_none() {
                args["summary"] = json!(cmd.summary.unwrap_or_else(|| {
                    "0/100 Progress not supplied; include task and summary".to_owned()
                }));
            }
        }
    }
    let get = |k: &str| {
        cut(
            args.get(k).and_then(Value::as_str).unwrap_or(""),
            if k == "workspace_id" {
                4096
            } else if k == "summary" {
                2048
            } else {
                80
            },
        )
    };
    let tool = cut(
        p.get("name").and_then(Value::as_str).unwrap_or("(invalid)"),
        80,
    );
    let d = db(config)?;
    d.execute("INSERT INTO calls(started_ms,status,tool,task,summary,workspace,input_size,arguments) VALUES(?1,'running',?2,?3,?4,?5,?6,?7)",params![now(),tool,get("task"),get("summary"),get("workspace_id"),original_args.to_string().len() as i64,encoded(config,&original_args)])?;
    Ok(d.last_insert_rowid())
}
pub fn finish(config: &Config, id: i64, output: &Value, error: bool, duration: u128) -> Result<()> {
    let d = db(config)?;
    d.execute("UPDATE calls SET finished_ms=?1,status=?2,duration_ms=?3,output_size=?4,output=?5 WHERE id=?6",params![now(),if error{"error"}else{"success"},duration as i64,output.to_string().len() as i64,encoded(config,output),id])?;
    d.execute("DELETE FROM calls WHERE id <= (SELECT COALESCE(MAX(id),0)-?1 FROM calls) AND status!='running'",[KEEP])?;
    Ok(())
}
#[derive(Default, Deserialize)]
pub struct Query {
    pub q: Option<String>,
    pub workspace: Option<String>,
    pub task: Option<String>,
    pub status: Option<String>,
    pub sort: Option<String>,
    pub offset: Option<usize>,
    pub snapshot: Option<i64>,
}
pub fn page(config: &Config, q: &Query) -> Result<Value> {
    let d = db(config)?;
    let snapshot =
        q.snapshot
            .unwrap_or(d.query_row("SELECT COALESCE(MAX(id),0) FROM calls", [], |r| r.get(0))?);
    let sort = match q.sort.as_deref() {
        Some("duration") => "COALESCE(duration_ms,0) DESC,id DESC",
        Some("input_size") => "input_size DESC,id DESC",
        Some("output_size") => "output_size DESC,id DESC",
        Some("oldest") => "id ASC",
        _ => "id DESC",
    };
    let text = cut(q.q.as_deref().unwrap_or(""), 200);
    let workspace = q.workspace.as_deref().unwrap_or("");
    let task = q.task.as_deref().unwrap_or("");
    let status = q.status.as_deref().unwrap_or("");
    let filter = "id<=?1 AND (?2='' OR instr(lower(tool||' '||task||' '||summary||' '||workspace),lower(?2))>0) AND (?3='' OR workspace=?3) AND (?4='' OR task=?4) AND (?5='' OR status=?5)";
    let total: i64 = d.query_row(
        &format!("SELECT count(*) FROM calls WHERE {filter}"),
        params![snapshot, text, workspace, task, status],
        |r| r.get(0),
    )?;
    let mut stmt=d.prepare(&format!("SELECT id,started_ms,finished_ms,status,tool,task,summary,workspace,duration_ms,input_size,output_size FROM calls WHERE {filter} ORDER BY {sort} LIMIT 100 OFFSET ?6"))?;
    let rows=stmt.query_map(params![snapshot,text,workspace,task,status,q.offset.unwrap_or(0).min(100000) as i64],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"started_ms":r.get::<_,i64>(1)?,"finished_ms":r.get::<_,Option<i64>>(2)?,"status":r.get::<_,String>(3)?,"tool":r.get::<_,String>(4)?,"task":r.get::<_,String>(5)?,"summary":r.get::<_,String>(6)?,"workspace":r.get::<_,String>(7)?,"duration_ms":r.get::<_,Option<i64>>(8)?,"input_size":r.get::<_,i64>(9)?,"output_size":r.get::<_,i64>(10)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(
        json!({"entries":rows,"total":total,"snapshot":snapshot,"retention":KEEP,"offset":q.offset.unwrap_or(0)}),
    )
}
pub fn detail(config: &Config, id: i64) -> Result<Value> {
    let d = db(config)?;
    let (args, out): (String, Option<String>) = d.query_row(
        "SELECT arguments,output FROM calls WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(
        json!({"id":id,"arguments":serde_json::from_str::<Value>(&args)?,"output":out.and_then(|s|serde_json::from_str::<Value>(&s).ok())}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn logs_redact_bound_and_persist() {
        let t = tempfile::tempdir().unwrap();
        let c = Config {
            database_path: t.path().join("db"),
            auth_token: Some("test-secret-token".into()),
            ..Config::default()
        };
        initialize(&c);
        let p = json!({"name":"bash","arguments":{"task":"test","command":"echo test-secret-token","password":"hidden"}});
        let id = begin(&c, p.as_object()).unwrap();
        finish(
            &c,
            id,
            &json!({"content":[{"type":"image","data":"secretimage"}],"text":"test-secret-token"}),
            false,
            8,
        )
        .unwrap();
        let v = detail(&c, id).unwrap().to_string();
        assert!(!v.contains("secretimage"));
        assert!(!v.contains("test-secret-token"));
        assert!(!v.contains("hidden"));
        assert_eq!(page(&c, &Query::default()).unwrap()["total"], 1);
        let id = begin(&c, p.as_object()).unwrap();
        initialize(&c);
        assert_eq!(
            page(&c, &Query::default()).unwrap()["entries"][0]["status"],
            "interrupted"
        );
        assert!(detail(&c, id).is_ok());
    }
}

//! Private, bounded, durable MCP invocation history. Never stores authentication headers or image payloads.
use crate::config::Config;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
const KEEP: i64 = 2000;
const KEEP_TASKS: i64 = 5000;
/// A task with no calls for this long starts a new numbered segment (NAME-2, ...).
pub const TASK_IDLE_MS: i64 = 5 * 60 * 1000;
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
    let has_task_id: i64 = db.query_row(
        "SELECT count(*) FROM pragma_table_info('calls') WHERE name='task_id'",
        [],
        |r| r.get(0),
    )?;
    if has_task_id == 0 {
        if let Err(error) = db.execute_batch("ALTER TABLE calls ADD COLUMN task_id INTEGER") {
            if !error.to_string().contains("duplicate column") {
                return Err(error.into());
            }
        }
    }
    db.execute_batch("CREATE TABLE IF NOT EXISTS tasks(id INTEGER PRIMARY KEY AUTOINCREMENT,base TEXT NOT NULL,name TEXT NOT NULL,seq INTEGER NOT NULL,first_ms INTEGER NOT NULL,last_ms INTEGER NOT NULL,calls INTEGER NOT NULL DEFAULT 0); CREATE INDEX IF NOT EXISTS tasks_base ON tasks(base,id); CREATE INDEX IF NOT EXISTS tasks_last ON tasks(last_ms); CREATE INDEX IF NOT EXISTS calls_task ON calls(task_id,status);")?;
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
/// Join the latest same-name task unless it has been idle for TASK_IDLE_MS
/// with no running call; otherwise start NAME-n timed from this call.
fn touch_task(db: &Connection, base: &str, at: i64) -> rusqlite::Result<(i64, String)> {
    let latest = db
        .query_row(
            "SELECT id,name,seq,last_ms,EXISTS(SELECT 1 FROM calls WHERE task_id=tasks.id AND status='running') FROM tasks WHERE base=?1 ORDER BY id DESC LIMIT 1",
            [base],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((id, name, _, last, running)) = &latest {
        if *running || at - *last <= TASK_IDLE_MS {
            db.execute(
                "UPDATE tasks SET last_ms=max(last_ms,?2),calls=calls+1 WHERE id=?1",
                params![id, at],
            )?;
            return Ok((*id, name.clone()));
        }
    }
    let seq = latest.map_or(1, |l| l.2 + 1);
    let name = if seq == 1 {
        base.to_owned()
    } else {
        format!("{base}-{seq}")
    };
    db.execute(
        "INSERT INTO tasks(base,name,seq,first_ms,last_ms,calls) VALUES(?1,?2,?3,?4,?4,1)",
        params![base, name, seq, at],
    )?;
    Ok((db.last_insert_rowid(), name))
}
pub fn begin(config: &Config, params: Option<&serde_json::Map<String, Value>>) -> Result<i64> {
    begin_at(config, params, now())
}
fn begin_at(
    config: &Config,
    params: Option<&serde_json::Map<String, Value>>,
    started: i64,
) -> Result<i64> {
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
                    "0/100 Quality score not supplied; include task and summary".to_owned()
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
    let task = get("task");
    let mut d = db(config)?;
    let tx = d.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (task_id, task) = if task.trim().is_empty() {
        (None, task)
    } else {
        let (id, name) = touch_task(&tx, &task, started)?;
        (Some(id), name)
    };
    tx.execute("INSERT INTO calls(started_ms,status,tool,task,summary,workspace,input_size,arguments,task_id) VALUES(?1,'running',?2,?3,?4,?5,?6,?7,?8)",params![started,tool,task,get("summary"),get("workspace_id"),original_args.to_string().len() as i64,encoded(config,&original_args),task_id])?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(id)
}
pub fn finish(config: &Config, id: i64, output: &Value, error: bool, duration: u128) -> Result<()> {
    finish_at(config, id, output, error, duration, now())
}
fn finish_at(
    config: &Config,
    id: i64,
    output: &Value,
    error: bool,
    duration: u128,
    finished: i64,
) -> Result<()> {
    let d = db(config)?;
    d.execute("UPDATE calls SET finished_ms=?1,status=?2,duration_ms=?3,output_size=?4,output=?5 WHERE id=?6",params![finished,if error{"error"}else{"success"},duration as i64,output.to_string().len() as i64,encoded(config,output),id])?;
    d.execute(
        "UPDATE tasks SET last_ms=max(last_ms,?1) WHERE id=(SELECT task_id FROM calls WHERE id=?2)",
        params![finished, id],
    )?;
    d.execute("DELETE FROM calls WHERE id <= (SELECT COALESCE(MAX(id),0)-?1 FROM calls) AND status!='running'",[KEEP])?;
    d.execute(
        "DELETE FROM tasks WHERE id <= (SELECT COALESCE(MAX(id),0)-?1 FROM tasks)",
        [KEEP_TASKS],
    )?;
    Ok(())
}
fn task_json(
    first: Option<i64>,
    last: Option<i64>,
    calls: Option<i64>,
    running: bool,
    at: i64,
) -> Value {
    match (first, last) {
        (Some(first), Some(last)) => json!({
            "task_first_ms":first,"task_last_ms":last,"task_total_ms":last-first,
            "task_calls":calls.unwrap_or(0),"task_running":running,
            "task_active":running || at-last <= TASK_IDLE_MS
        }),
        _ => {
            json!({"task_first_ms":null,"task_last_ms":null,"task_total_ms":null,"task_calls":null,"task_running":false,"task_active":false})
        }
    }
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
        Some("duration") => "COALESCE(c.duration_ms,0) DESC,c.id DESC",
        Some("input_size") => "c.input_size DESC,c.id DESC",
        Some("output_size") => "c.output_size DESC,c.id DESC",
        Some("oldest") => "c.id ASC",
        _ => "c.id DESC",
    };
    let text = cut(q.q.as_deref().unwrap_or(""), 200);
    let workspace = q.workspace.as_deref().unwrap_or("");
    let task = q.task.as_deref().unwrap_or("");
    let status = q.status.as_deref().unwrap_or("");
    let filter = "c.id<=?1 AND (?2='' OR instr(lower(c.tool||' '||c.task||' '||c.summary||' '||c.workspace),lower(?2))>0) AND (?3='' OR c.workspace=?3) AND (?4='' OR c.task=?4) AND (?5='' OR c.status=?5)";
    let at = now();
    let total: i64 = d.query_row(
        &format!("SELECT count(*) FROM calls c WHERE {filter}"),
        params![snapshot, text, workspace, task, status],
        |r| r.get(0),
    )?;
    let mut stmt=d.prepare(&format!("SELECT c.id,c.started_ms,c.finished_ms,c.status,c.tool,c.task,c.summary,c.workspace,c.duration_ms,c.input_size,c.output_size,t.first_ms,t.last_ms,t.calls,EXISTS(SELECT 1 FROM calls r WHERE r.task_id=t.id AND r.status='running') FROM calls c LEFT JOIN tasks t ON t.id=c.task_id WHERE {filter} ORDER BY {sort} LIMIT 100 OFFSET ?6"))?;
    let rows=stmt.query_map(params![snapshot,text,workspace,task,status,q.offset.unwrap_or(0).min(100000) as i64],|r|{
        let mut entry=json!({"id":r.get::<_,i64>(0)?,"started_ms":r.get::<_,i64>(1)?,"finished_ms":r.get::<_,Option<i64>>(2)?,"status":r.get::<_,String>(3)?,"tool":r.get::<_,String>(4)?,"task":r.get::<_,String>(5)?,"summary":r.get::<_,String>(6)?,"workspace":r.get::<_,String>(7)?,"duration_ms":r.get::<_,Option<i64>>(8)?,"input_size":r.get::<_,i64>(9)?,"output_size":r.get::<_,i64>(10)?});
        if let (Value::Object(entry), Value::Object(stats)) = (&mut entry, task_json(r.get(11)?,r.get(12)?,r.get(13)?,r.get(14)?,at)) {
            entry.extend(stats);
        }
        Ok(entry)
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stmt=d.prepare("SELECT t.name,t.first_ms,t.last_ms,t.calls,EXISTS(SELECT 1 FROM calls r WHERE r.task_id=t.id AND r.status='running'),(SELECT w.workspace FROM calls w WHERE w.task_id=t.id ORDER BY w.id DESC LIMIT 1) FROM tasks t WHERE (?1='' OR t.name=?1) AND (?2='' OR EXISTS(SELECT 1 FROM calls w WHERE w.task_id=t.id AND w.workspace=?2)) AND (?3='' OR instr(lower(t.name),lower(?3))>0) ORDER BY t.last_ms DESC,t.id DESC LIMIT 12")?;
    let tasks = stmt
        .query_map(params![task, workspace, text], |r| {
            let mut item =
                json!({"name":r.get::<_,String>(0)?,"workspace":r.get::<_,Option<String>>(5)?});
            if let (Value::Object(item), Value::Object(stats)) = (
                &mut item,
                task_json(r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, at),
            ) {
                item.extend(stats);
            }
            Ok(item)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(
        json!({"entries":rows,"tasks":tasks,"task_idle_ms":TASK_IDLE_MS,"now_ms":at,"total":total,"snapshot":snapshot,"retention":KEEP,"offset":q.offset.unwrap_or(0)}),
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
    #[test]
    fn tasks_track_time_calls_and_split_after_idle() {
        let t = tempfile::tempdir().unwrap();
        let c = Config {
            database_path: t.path().join("db"),
            ..Config::default()
        };
        let p = json!({"name":"bash","arguments":{"task":"Build","summary":"50/100 Building"}});
        let a = begin_at(&c, p.as_object(), 1_000).unwrap();
        finish_at(&c, a, &json!({}), false, 5, 2_000).unwrap();
        let b = begin_at(&c, p.as_object(), 61_000).unwrap();
        finish_at(&c, b, &json!({}), false, 5, 90_000).unwrap();
        // Idle past the window: a new numbered segment starts at this call.
        let x = begin_at(&c, p.as_object(), 90_001 + TASK_IDLE_MS).unwrap();
        // A still-running call keeps the segment alive past the idle window.
        let y = begin_at(&c, p.as_object(), 90_000 + 3 * TASK_IDLE_MS).unwrap();
        finish_at(&c, x, &json!({}), false, 5, 90_000 + 3 * TASK_IDLE_MS + 500).unwrap();
        let page_value = page(&c, &Query::default()).unwrap();
        let e = &page_value["entries"];
        assert_eq!(e[0]["id"], y);
        assert_eq!(e[0]["task"], "Build-2");
        assert_eq!(e[0]["task_calls"], 2);
        assert_eq!(e[0]["task_total_ms"], 2 * TASK_IDLE_MS + 499);
        assert_eq!(e[0]["task_running"], true);
        assert_eq!(e[2]["task"], "Build");
        assert_eq!(e[2]["task_calls"], 2);
        assert_eq!(e[2]["task_total_ms"], 89_000);
        assert_eq!(page_value["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(page_value["tasks"][0]["name"], "Build-2");
        let filtered = page(
            &c,
            &Query {
                task: Some("Build-2".into()),
                ..Query::default()
            },
        )
        .unwrap();
        assert_eq!(filtered["total"], 2);
        let other = json!({"name":"bash","arguments":{"task":"","summary":"50/100 Untracked"}});
        let u = begin_at(&c, other.as_object(), 1).unwrap();
        let untracked = page(&c, &Query::default()).unwrap();
        assert_eq!(untracked["entries"][0]["id"], u);
        assert!(untracked["entries"][0]["task_calls"].is_null());
    }
}

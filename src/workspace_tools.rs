//! Workspace activity (processes and listening ports) and read-only git status/diff.
use crate::{
    config::Config,
    db::{Database, canonical_workspace_path},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const CMDLINE_LIMIT: usize = 600;
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const STATUS_FILE_LIMIT: usize = 2000;
const DIFF_LIMIT: usize = 1024 * 1024;

/// A terminal shell that belongs to the workspace: (terminal id, terminal name, shell pid).
pub type TerminalShell = (i64, String, u32);

/// Processes started from the workspace's terminals, plus processes whose cwd is
/// inside the workspace (and not inside a more specific registered workspace).
/// `rows` are process-monitor snapshot rows, which already carry listening ports.
pub fn activity(
    config: &Config,
    workspace: &str,
    shells: &[TerminalShell],
    rows: &[Value],
    self_port: u16,
) -> Result<Value> {
    let db = Database::open_config(config)?;
    let entry = db.workspace(workspace)?;
    let root = canonical_workspace_path(config, &entry.path)?;
    let roots = db
        .list_workspaces()?
        .into_iter()
        .filter_map(|w| {
            canonical_workspace_path(config, &w.path)
                .ok()
                .map(|r| (w.id, r))
        })
        .collect::<Vec<_>>();
    let owner = |path: &Path| {
        roots
            .iter()
            .filter(|(_, r)| path.starts_with(r))
            .max_by_key(|(_, r)| r.as_os_str().len())
            .map(|(id, _)| *id)
    };
    let uid = unsafe { libc::geteuid() } as u64;
    let own_pid = u64::from(std::process::id());
    let rows = rows
        .iter()
        .filter(|row| row["uid"].as_u64() == Some(uid) && row["pid"].as_u64().is_some())
        // Protected rows are WebTerm itself and its supervisors.
        .filter(|row| row["can_control"] == true && row["pid"].as_u64() != Some(own_pid))
        .collect::<Vec<_>>();
    let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
    for row in &rows {
        if let (Some(pid), Some(ppid)) = (row["pid"].as_u64(), row["ppid"].as_u64()) {
            children.entry(ppid).or_default().push(pid);
        }
    }
    // Every descendant of a terminal shell is attributed to that terminal.
    let mut terminal_of: HashMap<u64, (i64, &str)> = HashMap::new();
    for (id, name, shell) in shells {
        let mut queue = VecDeque::from([u64::from(*shell)]);
        while let Some(pid) = queue.pop_front() {
            if terminal_of.insert(pid, (*id, name.as_str())).is_some() {
                continue;
            }
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    let mut processes = Vec::new();
    let mut ports = Vec::<Value>::new();
    let mut seen_ports = HashSet::new();
    for row in rows {
        let pid = row["pid"].as_u64().unwrap_or_default();
        let cwd = fs::read_link(format!("/proc/{pid}/cwd")).ok();
        let terminal = terminal_of.get(&pid).copied();
        let in_workspace = cwd.as_deref().and_then(owner) == Some(entry.id);
        if terminal.is_none() && !in_workspace {
            continue;
        }
        let mut process_ports = Vec::new();
        for port in row["ports"].as_array().into_iter().flatten() {
            if port["protocol"] != "tcp" {
                continue;
            }
            let Some(number) = port["port"]
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|p| crate::subdomain_proxy::valid_port(*p, self_port))
            else {
                continue;
            };
            if !process_ports.contains(&number) {
                process_ports.push(number);
            }
            if seen_ports.insert(number) {
                ports.push(json!({
                    "port": number,
                    "address": port["address"],
                    "pid": pid,
                    "name": row["name"],
                    "proxy_url": format!("/proxy/{number}/"),
                }));
            }
        }
        let relative_cwd = cwd
            .as_deref()
            .and_then(|c| c.strip_prefix(&root).ok())
            .map(|c| c.to_string_lossy().into_owned());
        processes.push(json!({
            "pid": pid,
            "ppid": row["ppid"],
            "name": row["name"],
            "state": row["state"],
            "start_time": row["start_time"],
            "cmdline": cmdline(pid),
            "cwd": cwd,
            "relative_cwd": relative_cwd,
            "terminal_id": terminal.map(|t| t.0),
            "terminal_name": terminal.map(|t| t.1),
            "ports": process_ports,
            "cpu_percent": row["cpu_percent"],
            "memory_bytes": row["memory_bytes"],
        }));
    }
    ports.sort_by_key(|p| p["port"].as_u64());
    // Listening processes first, then terminal jobs, then the rest by pid.
    processes.sort_by_key(|p| {
        (
            p["ports"].as_array().is_none_or(|v| v.is_empty()),
            p["terminal_id"].is_null(),
            p["pid"].as_u64(),
        )
    });
    Ok(json!({
        "workspace_id": entry.id,
        "path": root,
        "ports": ports,
        "processes": processes,
    }))
}

fn cmdline(pid: u64) -> String {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let mut text = raw
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(String::from_utf8_lossy)
        .collect::<Vec<_>>()
        .join(" ");
    if text.len() > CMDLINE_LIMIT {
        let mut end = CMDLINE_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

struct GitOutput {
    success: bool,
    stdout: Vec<u8>,
    truncated: bool,
}

/// Run git without prompts, optional locks, fsmonitor hooks, or external diff/textconv drivers.
fn git(dir: &Path, args: &[&str], limit: usize) -> Result<GitOutput> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=false",
            "--no-pager",
        ])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("git is not available")?;
    let mut stdout = child.stdout.take().context("git output unavailable")?;
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = (&mut stdout)
            .take(limit as u64 + 1)
            .read_to_end(&mut buffer);
        // Drain the rest so git is not blocked writing to a full pipe.
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        buffer
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > GIT_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut stdout = reader.join().unwrap_or_default();
    let truncated = stdout.len() > limit;
    stdout.truncate(limit);
    Ok(GitOutput {
        success: status.success(),
        stdout,
        truncated,
    })
}

/// Repository top level for a workspace, or None when it is not inside a git work tree.
fn toplevel(root: &Path) -> Result<Option<PathBuf>> {
    let output = git(root, &["rev-parse", "--show-toplevel"], 4096)?;
    if !output.success {
        return Ok(None);
    }
    let top = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(Some(PathBuf::from(top).canonicalize()?))
}

pub fn git_status(config: &Config, workspace: &str) -> Result<Value> {
    let root = crate::workspace_files::root(config, workspace)?;
    let Some(top) = toplevel(&root)? else {
        return Ok(json!({"repository": false, "path": root}));
    };
    // Paths are limited to the workspace but reported relative to the repository root.
    let output = git(
        &root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
        STATUS_OUTPUT_LIMIT,
    )?;
    if !output.success {
        bail!("git status failed");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut records = text.split('\0');
    let mut branch = json!({"head": null, "oid": null, "upstream": null, "ahead": 0, "behind": 0});
    let mut files = Vec::new();
    let mut truncated = output.truncated;
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if let Some(header) = record.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            match key {
                "branch.head" => branch["head"] = json!(value),
                "branch.oid" => branch["oid"] = json!(value),
                "branch.upstream" => branch["upstream"] = json!(value),
                "branch.ab" => {
                    for part in value.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+').and_then(|n| n.parse::<u64>().ok())
                        {
                            branch["ahead"] = json!(n);
                        } else if let Some(n) =
                            part.strip_prefix('-').and_then(|n| n.parse::<u64>().ok())
                        {
                            branch["behind"] = json!(n);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let (kind, rest) = record.split_at(1);
        let rest = rest.trim_start();
        let file = match kind {
            // 1 XY sub mH mI mW hH hI path
            "1" => rest.splitn(8, ' ').collect::<Vec<_>>().get(7).map(|path| status_entry(rest, path, None)),
            // 2 XY sub mH mI mW hH hI Xscore path\0orig
            "2" => {
                let path = rest.splitn(9, ' ').nth(8).unwrap_or_default().to_owned();
                let orig = records.next().map(str::to_owned);
                Some(status_entry(rest, &path, orig.as_deref()))
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            "u" => rest
                .splitn(10, ' ')
                .nth(9)
                .map(|path| json!({"path": path, "orig_path": null, "index": "U", "worktree": "U", "status": "conflict"})),
            "?" => Some(json!({"path": rest, "orig_path": null, "index": "?", "worktree": "?", "status": "untracked"})),
            _ => None,
        };
        if let Some(file) = file {
            if files.len() >= STATUS_FILE_LIMIT {
                truncated = true;
                break;
            }
            files.push(file);
        }
    }
    let relative_root = root
        .strip_prefix(&top)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    Ok(json!({
        "repository": true,
        "path": root,
        "toplevel": top,
        "workspace_prefix": relative_root,
        "branch": branch,
        "files": files,
        "truncated": truncated,
    }))
}

fn status_entry(rest: &str, path: &str, orig: Option<&str>) -> Value {
    let xy = rest.get(..2).unwrap_or("..");
    let (index, worktree) = xy.split_at(1);
    let code = if index != "." { index } else { worktree };
    let status = match code {
        "A" => "added",
        "D" => "deleted",
        "R" => "renamed",
        "C" => "copied",
        "T" => "type-changed",
        _ => "modified",
    };
    json!({"path": path, "orig_path": orig, "index": index, "worktree": worktree, "status": status})
}

/// Diff of one file against HEAD (staged and unstaged changes together).
/// Untracked files are shown as a whole-file addition.
pub fn git_diff(config: &Config, workspace: &str, path: &str) -> Result<Value> {
    let relative = Path::new(path);
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\0')
        || !relative
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("invalid file path");
    }
    let root = crate::workspace_files::root(config, workspace)?;
    let top = toplevel(&root)?.context("workspace is not a git repository")?;
    if !top.join(relative).starts_with(&root) {
        bail!("file is outside the workspace");
    }
    let flags = [
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
    ];
    let tracked = git(&top, &["ls-files", "--error-unmatch", "--", path], 4096)?.success;
    let output = if !tracked {
        let mut args = vec!["diff", "--no-index"];
        args.extend(flags);
        args.extend(["--", "/dev/null", path]);
        git(&top, &args, DIFF_LIMIT)?
    } else {
        let mut args = vec!["diff"];
        args.extend(flags);
        args.extend(["HEAD", "--", path]);
        let head = git(&top, &args, DIFF_LIMIT)?;
        if head.success {
            head
        } else {
            // No commits yet: everything tracked is staged.
            let mut args = vec!["diff", "--cached"];
            args.extend(flags);
            args.extend(["--", path]);
            git(&top, &args, DIFF_LIMIT)?
        }
    };
    Ok(json!({
        "path": path,
        "untracked": !tracked,
        "diff": String::from_utf8_lossy(&output.stdout),
        "truncated": output.truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_entries_use_the_most_significant_change() {
        let entry = status_entry(
            "M. N... 100644 100644 100644 a b src/x.rs",
            "src/x.rs",
            None,
        );
        assert_eq!(entry["status"], "modified");
        assert_eq!(entry["index"], "M");
        let entry = status_entry(".D N... 100644 100644 000000 a b gone.rs", "gone.rs", None);
        assert_eq!(entry["status"], "deleted");
        assert_eq!(entry["worktree"], "D");
    }

    #[test]
    fn cmdline_of_missing_process_is_empty() {
        assert_eq!(cmdline(u64::MAX), "");
    }
}

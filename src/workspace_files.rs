//! Read-only workspace explorer. Resolve and verify opened descriptors before reading.
use crate::{
    config::Config,
    db::{Database, canonical_workspace_path},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Read,
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
pub const TEXT_LIMIT: u64 = 256 * 1024;
pub const PAGE_SIZE: usize = 500;

pub fn root(config: &Config, workspace: &str) -> Result<PathBuf> {
    let db = Database::open_config(config)?;
    let entry = if workspace.starts_with('/') {
        db.list_workspaces()?
            .into_iter()
            .find(|w| w.path == Path::new(workspace))
            .context("workspace not found")?
    } else {
        db.workspace(workspace)?
    };
    canonical_workspace_path(config, &entry.path)
}

pub fn open(config: &Config, workspace: &str, requested: &str) -> Result<(File, PathBuf, PathBuf)> {
    if requested.len() > 4096 || requested.contains('\0') {
        bail!("invalid file path");
    }
    let root = root(config, workspace)?;
    let path = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        root.join(requested)
    };
    let canonical = path
        .canonicalize()
        .context("file not found or inaccessible")?;
    if !canonical.starts_with(&root) {
        bail!("path escapes workspace");
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&canonical)
        .context("open file")?;
    let actual = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if actual != canonical || !actual.starts_with(&root) {
        bail!("path changed during open; retry");
    }
    let meta = file.metadata()?;
    if !meta.is_file() && !meta.is_dir() {
        bail!("only regular files and folders are supported");
    }
    Ok((file, canonical, root))
}

pub fn kind(path: &Path) -> (&'static str, &'static str) {
    match path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => ("image", "image/png"),
        "jpg" | "jpeg" => ("image", "image/jpeg"),
        "gif" => ("image", "image/gif"),
        "webp" => ("image", "image/webp"),
        "avif" => ("image", "image/avif"),
        "svg" => ("image", "image/svg+xml"),
        "mp3" => ("audio", "audio/mpeg"),
        "wav" => ("audio", "audio/wav"),
        "ogg" | "oga" => ("audio", "audio/ogg"),
        "m4a" | "aac" => ("audio", "audio/mp4"),
        "flac" => ("audio", "audio/flac"),
        "opus" => ("audio", "audio/ogg"),
        "mp4" | "m4v" => ("video", "video/mp4"),
        "webm" => ("video", "video/webm"),
        "ogv" => ("video", "video/ogg"),
        "mov" => ("video", "video/quicktime"),
        "glb" => ("model", "model/gltf-binary"),
        "gltf" => ("model", "model/gltf+json"),
        "html" | "htm" => ("html", "text/html; charset=utf-8"),
        "css" => ("text", "text/css; charset=utf-8"),
        "js" | "mjs" => ("text", "text/javascript; charset=utf-8"),
        "json" => ("text", "application/json"),
        "woff" => ("binary", "font/woff"),
        "woff2" => ("binary", "font/woff2"),
        "ttf" => ("binary", "font/ttf"),
        "otf" => ("binary", "font/otf"),
        "wasm" => ("binary", "application/wasm"),
        "bin" => ("binary", "application/octet-stream"),
        "pdf" => ("binary", "application/pdf"),
        _ => ("text", "text/plain; charset=utf-8"),
    }
}

fn modified(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|v| v.duration_since(UNIX_EPOCH).ok())
        .map(|v| v.as_millis() as u64)
        .unwrap_or(0)
}

pub fn listing(config: &Config, workspace: &str, path: &str, offset: usize) -> Result<Value> {
    let (dir, canonical, root) = open(config, workspace, path)?;
    if !dir.metadata()?.is_dir() {
        bail!("path is not a folder");
    }
    // Enumerate the verified directory descriptor so a path rename cannot redirect the read.
    let descriptor = format!("/proc/self/fd/{}", dir.as_raw_fd());
    let mut entries = Vec::new();
    for item in fs::read_dir(descriptor)? {
        let Ok(item) = item else { continue };
        let Some(name) = item.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let target = canonical.join(&name);
        let Ok(real) = target.canonicalize() else {
            continue;
        };
        if !real.starts_with(&root) {
            continue;
        }
        let Ok(meta) = fs::metadata(&real) else {
            continue;
        };
        if !meta.is_dir() && !meta.is_file() {
            continue;
        }
        let (kind, mime) = kind(&target);
        entries.push(json!({"name":name,"path":target,"relative_path":target.strip_prefix(&root)?,"is_dir":meta.is_dir(),"is_symlink":item.file_type().map(|x|x.is_symlink()).unwrap_or(false),"size":meta.len(),"modified_ms":modified(&meta),"kind":if meta.is_dir(){"folder"}else{kind},"mime_type":mime}));
    }
    // Page recent entries first so newly created files are discoverable even in large directories.
    // The browser groups ordinary entries by folder/name and pins newly observed paths above them.
    entries.sort_by(|a, b| {
        b["modified_ms"]
            .as_u64()
            .cmp(&a["modified_ms"].as_u64())
            .then_with(|| a["name"].as_str().unwrap().cmp(b["name"].as_str().unwrap()))
    });
    let total = entries.len();
    let page = entries
        .into_iter()
        .skip(offset)
        .take(PAGE_SIZE)
        .collect::<Vec<_>>();
    let next = offset.saturating_add(page.len());
    Ok(
        json!({"path":canonical,"workspace_path":root,"entries":page,"total":total,"next_offset":if next<total{Some(next)}else{None},"refresh_ms":5000}),
    )
}

pub fn preview(config: &Config, workspace: &str, path: &str) -> Result<Value> {
    let (mut file, canonical, root) = open(config, workspace, path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("preview requires a regular file");
    }
    let (mut kind, mime) = kind(&canonical);
    let mut result = json!({"path":canonical,"relative_path":canonical.strip_prefix(&root)?,"workspace_path":root,"name":canonical.file_name().unwrap().to_string_lossy(),"size":meta.len(),"modified_ms":modified(&meta),"kind":kind,"mime_type":mime});
    if kind == "text" {
        let mut bytes = Vec::new();
        (&mut file).take(TEXT_LIMIT + 1).read_to_end(&mut bytes)?;
        if bytes.iter().take(8192).any(|b| *b == 0) {
            kind = "binary";
            result["kind"] = json!(kind);
        } else {
            let truncated = bytes.len() as u64 > TEXT_LIMIT;
            bytes.truncate(TEXT_LIMIT as usize);
            result["text"] = json!(String::from_utf8_lossy(&bytes));
            result["truncated"] = json!(truncated);
            result["text_limit"] = json!(TEXT_LIMIT);
        }
    }
    if kind == "video" || kind == "audio" {
        if let Some(media) = probe_media(&canonical) {
            result["media"] = media;
        }
    }
    Ok(result)
}

/// Current directory of the terminal's foreground job (e.g. an editor or CLI
/// started from the shell), falling back to the shell itself.
pub fn foreground_cwd(pid: u32) -> Option<PathBuf> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised command: state ppid pgrp session tty_nr tpgid.
    let tpgid = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(5)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0);
    tpgid
        .into_iter()
        .map(|p| p as u64)
        .chain([u64::from(pid)])
        .find_map(|p| fs::read_link(format!("/proc/{p}/cwd")).ok())
}

const RESOLVE_LIMIT: usize = 50;
const SEARCH_ENTRY_LIMIT: usize = 200_000;
const SEARCH_TIME_LIMIT: std::time::Duration = std::time::Duration::from_millis(1500);
const SEARCH_SKIP: [&str; 7] = [
    ".git",
    "node_modules",
    "target",
    ".venv",
    "__pycache__",
    ".cache",
    ".npm",
];

/// Resolve text clicked in a terminal to workspace files. Exact paths are tried
/// relative to the terminal's cwd and the workspace; otherwise the workspace is
/// searched for files with the same name (and matching trailing path segments).
pub fn resolve(config: &Config, workspace: &str, cwd: Option<&Path>, text: &str) -> Result<Value> {
    let text = text.trim();
    let text = text.strip_prefix("file://").unwrap_or(text);
    if text.is_empty() || text.len() > 4096 || text.contains('\0') {
        bail!("invalid file path");
    }
    let db = Database::open_config(config)?;
    let roots = db
        .list_workspaces()?
        .into_iter()
        .filter_map(|w| {
            canonical_workspace_path(config, &w.path)
                .ok()
                .map(|root| (w.id, root))
        })
        .collect::<Vec<_>>();
    let root = root(config, workspace)?;
    let owner = |path: &Path| {
        roots
            .iter()
            .filter(|(_, r)| path.starts_with(r))
            .max_by_key(|(_, r)| r.as_os_str().len())
            .cloned()
    };
    let requested = match text.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)),
        None => Some(PathBuf::from(text)),
    };
    let mut candidates = Vec::new();
    if let Some(requested) = requested {
        if requested.is_absolute() {
            candidates.push(requested);
        } else {
            if let Some(cwd) = cwd {
                candidates.push(cwd.join(&requested));
            }
            candidates.push(root.join(&requested));
        }
    }
    for candidate in candidates {
        let Ok(canonical) = candidate.canonicalize() else {
            continue;
        };
        let Some((id, owner_root)) = owner(&canonical) else {
            continue;
        };
        if let Some(entry) = resolved_entry(id, &owner_root, &canonical, true) {
            return Ok(json!({"query":text,"exact":true,"matches":[entry],"truncated":false}));
        }
    }

    // Not found as a path: search the workspace by file name and trailing segments.
    let segments = Path::new(text)
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(v) => Some(v.to_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some(name) = segments.last().cloned() else {
        return Ok(json!({"query":text,"exact":false,"matches":[],"truncated":false}));
    };
    let suffix = segments.iter().collect::<PathBuf>();
    // Hidden trees (.cargo, .local, .rustup...) dominate home workspaces; enter them only when asked.
    let hidden_query = segments
        .iter()
        .any(|s| s.to_string_lossy().starts_with('.'));
    // The shell's cwd is searched first so nearby files rank above the rest of the workspace.
    let mut starts = Vec::new();
    if let Some(cwd) = cwd.and_then(|c| c.canonicalize().ok()) {
        if let Some((_, cwd_root)) = owner(&cwd) {
            starts.push(cwd);
            starts.push(cwd_root);
        }
    }
    starts.push(root.clone());
    let started = std::time::Instant::now();
    let mut matches = Vec::new();
    let mut seen_dirs = std::collections::HashSet::new();
    let mut visited = 0usize;
    let mut truncated = false;
    'walk: for start in starts {
        let mut queue = std::collections::VecDeque::from([start]);
        while let Some(dir) = queue.pop_front() {
            if !seen_dirs.insert(dir.clone()) {
                continue;
            }
            let Ok(items) = fs::read_dir(&dir) else {
                continue;
            };
            let mut children = Vec::new();
            for item in items.flatten() {
                visited += 1;
                if visited > SEARCH_ENTRY_LIMIT || started.elapsed() > SEARCH_TIME_LIMIT {
                    truncated = true;
                    break 'walk;
                }
                let Ok(file_type) = item.file_type() else {
                    continue;
                };
                let file_name = item.file_name();
                if file_type.is_dir() {
                    let hidden = file_name.to_string_lossy().starts_with('.');
                    if !SEARCH_SKIP.iter().any(|skip| file_name == *skip)
                        && (hidden_query || !hidden)
                    {
                        children.push(item.path());
                    }
                    continue;
                }
                if file_name != name {
                    continue;
                }
                let path = item.path();
                if !path.ends_with(&suffix) {
                    continue;
                }
                let Ok(canonical) = path.canonicalize() else {
                    continue;
                };
                if matches
                    .iter()
                    .any(|m: &Value| m["path"] == json!(canonical))
                {
                    continue;
                }
                let Some((id, owner_root)) = owner(&canonical) else {
                    continue;
                };
                if let Some(entry) = resolved_entry(id, &owner_root, &canonical, false) {
                    matches.push(entry);
                    if matches.len() >= RESOLVE_LIMIT {
                        truncated = true;
                        break 'walk;
                    }
                }
            }
            children.sort();
            queue.extend(children);
        }
    }
    Ok(json!({"query":text,"exact":false,"matches":matches,"truncated":truncated}))
}

fn resolved_entry(
    workspace_id: i64,
    root: &Path,
    canonical: &Path,
    allow_dir: bool,
) -> Option<Value> {
    let relative = canonical.strip_prefix(root).ok()?;
    let meta = fs::metadata(canonical).ok()?;
    if !(meta.is_file() || (allow_dir && meta.is_dir())) {
        return None;
    }
    let (kind, mime) = kind(canonical);
    Some(json!({
        "workspace_id": workspace_id.to_string(),
        "workspace_path": root,
        "path": canonical,
        "relative_path": relative,
        "name": canonical.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        "is_dir": meta.is_dir(),
        "size": meta.len(),
        "modified_ms": modified(&meta),
        "kind": if meta.is_dir() { "folder" } else { kind },
        "mime_type": mime,
    }))
}

/// Best-effort codec/resolution/bitrate via ffprobe when it is installed.
fn probe_media(path: &std::path::Path) -> Option<Value> {
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "--",
        ])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let probe: Value = serde_json::from_slice(&output.stdout).ok()?;
    let streams = probe["streams"].as_array()?;
    let stream = |kind: &str| streams.iter().find(|s| s["codec_type"] == kind);
    let number = |v: &Value| {
        v.as_str()
            .and_then(|x| x.parse::<f64>().ok())
            .or_else(|| v.as_f64())
    };
    let video = stream("video");
    let audio = stream("audio");
    Some(json!({
        "width": video.and_then(|v| v["width"].as_u64()),
        "height": video.and_then(|v| v["height"].as_u64()),
        "video_codec": video.and_then(|v| v["codec_name"].as_str()),
        "audio_codec": audio.and_then(|v| v["codec_name"].as_str()),
        "frame_rate": video.and_then(|v| v["avg_frame_rate"].as_str()),
        "duration_s": number(&probe["format"]["duration"]),
        "bit_rate": number(&probe["format"]["bit_rate"]),
        "video_bit_rate": video.and_then(|v| number(&v["bit_rate"])),
    }))
}

/// A single RFC 9110 byte range, including suffix and open-ended ranges.
pub fn byte_range(value: Option<&str>, length: u64) -> Result<Option<(u64, u64)>> {
    let Some(value) = value else { return Ok(None) };
    let spec = value
        .strip_prefix("bytes=")
        .context("unsupported range unit")?;
    if spec.contains(',') || length == 0 {
        bail!("range not satisfiable");
    }
    let (start, end) = spec.split_once('-').context("invalid byte range")?;
    let (start, end) = if start.is_empty() {
        let suffix = end.parse::<u64>()?;
        if suffix == 0 {
            bail!("invalid suffix range");
        }
        (length.saturating_sub(suffix), length - 1)
    } else {
        let start = start.parse::<u64>()?;
        let end = if end.is_empty() {
            length - 1
        } else {
            end.parse::<u64>()?.min(length - 1)
        };
        (start, end)
    };
    if start >= length || start > end {
        bail!("range not satisfiable");
    }
    Ok(Some((start, end)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Config, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let c = Config {
            database_path: temp.path().join("db"),
            workspace_roots: vec![root.clone()],
            ..Config::default()
        };
        Database::open_config(&c)
            .unwrap()
            .ensure_workspace(&root)
            .unwrap();
        (temp, c, root.to_str().unwrap().into())
    }
    #[test]
    fn confinement_and_special_files() {
        let (t, c, w) = setup();
        fs::write(t.path().join("secret"), "no").unwrap();
        std::os::unix::fs::symlink(t.path().join("secret"), Path::new(&w).join("escape")).unwrap();
        assert!(open(&c, &w, "../secret").is_err());
        assert!(open(&c, &w, "escape").is_err());
        assert!(open(&c, "/etc", "passwd").is_err());
        assert!(open(&c, &w, "\0").is_err());
    }
    #[test]
    fn hidden_unicode_nested_and_limits() {
        let (_t, c, w) = setup();
        fs::write(Path::new(&w).join(".hidden"), "hello").unwrap();
        fs::create_dir(Path::new(&w).join("sub folder")).unwrap();
        fs::write(
            Path::new(&w).join("sub folder/你好.txt"),
            vec![b'x'; TEXT_LIMIT as usize + 1],
        )
        .unwrap();
        let p = preview(&c, &w, "sub folder/你好.txt").unwrap();
        assert_eq!(p["truncated"], true);
        assert_eq!(p["text"].as_str().unwrap().len(), TEXT_LIMIT as usize);
        let l = listing(&c, &w, "", 0).unwrap();
        assert_eq!(l["entries"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn resolve_exact_and_search() {
        let (_t, c, w) = setup();
        fs::create_dir_all(Path::new(&w).join("src/deep")).unwrap();
        fs::write(Path::new(&w).join("src/deep/main.rs"), "fn main() {}").unwrap();
        let exact = resolve(&c, &w, None, "src/deep/main.rs").unwrap();
        assert_eq!(exact["exact"], true);
        assert_eq!(exact["matches"][0]["relative_path"], "src/deep/main.rs");
        let cwd = Path::new(&w).join("src");
        let from_cwd = resolve(&c, &w, Some(&cwd), "deep/main.rs").unwrap();
        assert_eq!(from_cwd["exact"], true);
        let searched = resolve(&c, &w, None, "main.rs").unwrap();
        assert_eq!(searched["exact"], false);
        assert_eq!(searched["matches"].as_array().unwrap().len(), 1);
        let suffix = resolve(&c, &w, None, "other/deep/main.rs").unwrap();
        assert!(suffix["matches"].as_array().unwrap().is_empty());
        assert!(
            resolve(&c, &w, None, "missing.txt").unwrap()["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            resolve(&c, &w, None, "/etc/passwd").unwrap()["matches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn ranges() {
        assert_eq!(byte_range(Some("bytes=2-4"), 10).unwrap(), Some((2, 4)));
        assert_eq!(byte_range(Some("bytes=-3"), 10).unwrap(), Some((7, 9)));
        assert_eq!(byte_range(Some("bytes=5-"), 10).unwrap(), Some((5, 9)));
        for r in ["bytes=20-", "bytes=5-2", "bytes=0-1,4-5", "bytes=-0", "bad"] {
            assert!(byte_range(Some(r), 10).is_err());
        }
    }
    #[test]
    fn media_types() {
        for (f, k) in [
            ("a.glb", "model"),
            ("a.wav", "audio"),
            ("a.mp4", "video"),
            ("a.webp", "image"),
            ("a.html", "html"),
        ] {
            assert_eq!(kind(Path::new(f)).0, k);
        }
    }
}

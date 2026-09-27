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

/// Best-effort codec/resolution/bitrate via ffprobe when it is installed.
fn probe_media(path: &std::path::Path) -> Option<Value> {
    let output = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams", "--"])
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
    let number = |v: &Value| v.as_str().and_then(|x| x.parse::<f64>().ok()).or_else(|| v.as_f64());
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

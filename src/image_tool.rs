//! Read one bounded workspace image and return an actual MCP ImageContent block.
use crate::{config::Config, db::canonical_workspace_path};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader, Limits};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Cursor, Read},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PIXELS: u64 = 32 * 1024 * 1024;

pub fn read(config: &Config, workspace: &str, path: &str) -> Result<Value> {
    if !workspace.starts_with('/') || workspace.len() > 4096 || workspace.contains('\0') {
        bail!("workspace_id must be an absolute folder path");
    }
    if path.is_empty() || path.len() > 4096 || path.contains('\0') {
        bail!("path must name a workspace image");
    }
    let root = canonical_workspace_path(config, Path::new(workspace))?;
    let target = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        root.join(path)
    };
    let canonical = target
        .canonicalize()
        .context("image does not exist or cannot be resolved")?;
    if !canonical.starts_with(&root) {
        bail!("image path escapes workspace_id");
    }
    // O_NONBLOCK prevents special files from hanging open; final symlink swaps fail.
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&canonical)
        .context("open workspace image")?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("image must be a regular file");
    }
    if meta.len() == 0 || meta.len() > MAX_BYTES {
        bail!("image must be nonempty and no larger than 16 MiB");
    }
    // Verify the opened descriptor, not only a path checked before open.
    let opened = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .context("verify opened image path")?;
    if !opened.starts_with(&root) || opened != canonical {
        bail!("image path changed during open");
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    (&mut file).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() as u64 > MAX_BYTES
        || after.len() != meta.len()
        || after.mtime() != meta.mtime()
        || after.mtime_nsec() != meta.mtime_nsec()
    {
        bail!("image changed during read; retry after writing finishes");
    }
    let format = image::guess_format(&bytes).context("file is not a supported image")?;
    let mime = match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => bail!("supported images are PNG, JPEG, GIF and WebP"),
    };
    let reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let (width, height) = reader.into_dimensions().context("invalid image header")?;
    if width == 0
        || height == 0
        || width > 16384
        || height > 16384
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        bail!("image exceeds dimension limit (16384 per side, 32 megapixels)");
    }
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    // Validate the first frame before exposing bytes; image extensions are not trusted.
    reader
        .decode()
        .context("image is corrupt or exceeds decoder limits")?;
    let data = json!({"workspace_id":root,"path":canonical,"mime_type":mime,"bytes":bytes.len(),"width":width,"height":height});
    Ok(
        json!({"isError":false,"structuredContent":data,"content":[{"type":"text","text":serde_json::to_string(&data)?},{"type":"image","mimeType":mime,"data":STANDARD.encode(&bytes)}]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbaImage};
    use tempfile::TempDir;
    fn setup() -> (TempDir, Config) {
        let dir = TempDir::new().unwrap();
        let c = Config {
            workspace_roots: vec![dir.path().to_path_buf()],
            ..Config::default()
        };
        (dir, c)
    }
    fn png() -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(RgbaImage::new(3, 2))
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }
    #[test]
    fn image_block_round_trip() {
        let (d, c) = setup();
        let raw = png();
        fs::write(d.path().join("different.ext"), &raw).unwrap();
        let v = read(&c, d.path().to_str().unwrap(), "different.ext").unwrap();
        assert_eq!(v["structuredContent"]["width"], 3);
        assert_eq!(v["content"][1]["mimeType"], "image/png");
        assert_eq!(
            STANDARD
                .decode(v["content"][1]["data"].as_str().unwrap())
                .unwrap(),
            raw
        );
    }
    #[test]
    fn rejects_escape_and_symlink_escape() {
        let (d, c) = setup();
        let outside = TempDir::new().unwrap();
        let p = outside.path().join("x.png");
        fs::write(&p, png()).unwrap();
        assert!(read(&c, d.path().to_str().unwrap(), p.to_str().unwrap()).is_err());
        std::os::unix::fs::symlink(&p, d.path().join("escape")).unwrap();
        assert!(read(&c, d.path().to_str().unwrap(), "escape").is_err());
    }
    #[test]
    fn rejects_nonimages_empty_directory_and_oversized() {
        let (d, c) = setup();
        let root = d.path().to_str().unwrap();
        for (name, data) in [("fake.png", b"not an image".to_vec()), ("empty", vec![])] {
            fs::write(d.path().join(name), data).unwrap();
            assert!(read(&c, root, name).is_err());
        }
        assert!(read(&c, root, ".").is_err());
        let f = fs::File::create(d.path().join("big")).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        assert!(read(&c, root, "big").is_err());
    }
    #[test]
    fn every_supported_format_decodes() {
        let (d, c) = setup();
        for (f, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::Gif, "image/gif"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let mut out = Cursor::new(Vec::new());
            DynamicImage::new_rgb8(4, 3).write_to(&mut out, f).unwrap();
            fs::write(d.path().join("fixture"), out.into_inner()).unwrap();
            let v = read(&c, d.path().to_str().unwrap(), "fixture").unwrap();
            assert_eq!(v["structuredContent"]["mime_type"], mime);
        }
    }
    #[test]
    fn rejects_corrupt_image() {
        let (d, c) = setup();
        let mut raw = png();
        raw.truncate(35);
        fs::write(d.path().join("bad.png"), raw).unwrap();
        assert!(read(&c, d.path().to_str().unwrap(), "bad.png").is_err());
    }
    #[test]
    fn accepts_internal_symlink() {
        let (d, c) = setup();
        fs::write(d.path().join("x.png"), png()).unwrap();
        std::os::unix::fs::symlink("x.png", d.path().join("link")).unwrap();
        assert!(read(&c, d.path().to_str().unwrap(), "link").is_ok());
    }
}

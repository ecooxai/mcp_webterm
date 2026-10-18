//! Unguessable, expiring links that serve one folder without a login, for previews opened in a new tab.
use rand_core::{OsRng, RngCore};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const TTL: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_LINKS: usize = 4096;

fn links() -> &'static Mutex<HashMap<String, (PathBuf, Instant)>> {
    static LINKS: OnceLock<Mutex<HashMap<String, (PathBuf, Instant)>>> = OnceLock::new();
    LINKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Returns a token granting read access to `folder` and everything below it until it expires.
pub fn create(folder: &Path) -> String {
    let mut bytes = [0u8; 24];
    OsRng.fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let now = Instant::now();
    let mut map = links().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, (_, expires)| *expires > now);
    if map.len() >= MAX_LINKS {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, (_, e))| *e)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    map.insert(token.clone(), (folder.to_path_buf(), now + TTL));
    token
}

/// Folder shared under `token`, if the token exists and has not expired.
pub fn folder(token: &str) -> Option<PathBuf> {
    let map = links().lock().unwrap_or_else(|e| e.into_inner());
    map.get(token)
        .filter(|(_, expires)| *expires > Instant::now())
        .map(|(folder, _)| folder.clone())
}

#[cfg(test)]
mod tests {
    #[test]
    fn tokens_resolve_only_to_their_folder() {
        let token = super::create(std::path::Path::new("/tmp/share-test"));
        assert_eq!(token.len(), 48);
        assert_eq!(
            super::folder(&token).unwrap(),
            std::path::Path::new("/tmp/share-test")
        );
        assert!(super::folder("0000").is_none());
    }
}

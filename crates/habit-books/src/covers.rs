//! Cover images: a book's `cover_url` is either a web URL (downloaded once by
//! habitd into `$XDG_CACHE_HOME/habitfocus/covers/`) or a local file path.
//! Clients only read the files; [`path`] tells them where to look.

use std::path::PathBuf;

pub fn cache_dir() -> PathBuf {
    let cache = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into())).join(".cache")
    });
    cache.join("habitfocus").join("covers")
}

/// Where the cover of `url` is (or will be once downloaded).
pub fn path(url: &str) -> PathBuf {
    if let Some(path) = local_path(url) {
        return path;
    }
    let ext = url.rsplit('.').next().filter(|e| e.len() <= 4 && !e.contains('/')).unwrap_or("img");
    cache_dir().join(format!("{:016x}.{ext}", fnv1a(url)))
}

/// Local path of the cover, downloading it first if needed. Blocking.
#[cfg(feature = "store")]
pub fn fetch(url: &str) -> anyhow::Result<PathBuf> {
    let path = path(url);
    if local_path(url).is_some() {
        anyhow::ensure!(path.is_file(), "{} not found", path.display());
        return Ok(path);
    }
    if !path.is_file() {
        std::fs::create_dir_all(cache_dir())?;
        crate::http::download(url, &path)?;
    }
    Ok(path)
}

fn local_path(url: &str) -> Option<PathBuf> {
    if let Some(p) = url.strip_prefix("file://") {
        return Some(PathBuf::from(p));
    }
    if let Some(p) = url.strip_prefix("~/") {
        return std::env::var_os("HOME").map(|h| PathBuf::from(h).join(p));
    }
    url.starts_with('/').then(|| PathBuf::from(url))
}

/// Stable across Rust versions, unlike `DefaultHasher`, so cache names survive upgrades.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_local_paths() {
        assert_eq!(local_path("/tmp/a.png"), Some(PathBuf::from("/tmp/a.png")));
        assert_eq!(local_path("file:///tmp/a.png"), Some(PathBuf::from("/tmp/a.png")));
        assert_eq!(local_path("https://covers.openlibrary.org/b/id/1-L.jpg"), None);
        assert_eq!(path("/tmp/a.png"), PathBuf::from("/tmp/a.png"));
        assert!(path("https://covers.openlibrary.org/b/id/1-L.jpg").ends_with(format!(
            "{:016x}.jpg",
            fnv1a("https://covers.openlibrary.org/b/id/1-L.jpg")
        )));
    }

    #[cfg(feature = "store")]
    #[test]
    fn missing_local_file_is_an_error() {
        assert!(fetch("/definitely/not/here.png").is_err());
    }
}

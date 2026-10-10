//! Cover images: a book's `cover_url` is a web URL (downloaded once by habitd
//! into `$XDG_CACHE_HOME/habitfocus/covers/`), an image of its own as a `data:`
//! URL (written to the same cache), or a local file path. Clients only read
//! the files; [`path`] tells them where to look.
//!
//! Own images travel with the book when it syncs, so they are kept small:
//! [`embed`] turns a local file into such a URL when a book is saved.

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
    let ext = match data_mime(url) {
        Some("image/png") => "png",
        Some(_) => "jpg",
        None => url.rsplit('.').next().filter(|e| e.len() <= 4 && !e.contains('/')).unwrap_or("img"),
    };
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
        match data_mime(url) {
            Some(_) => {
                let bytes = decode_data(url)?;
                let tmp = path.with_extension("part");
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, &path)?;
            }
            None => crate::http::download(url, &path)?,
        }
    }
    Ok(path)
}

/// Widest an own cover is kept, in pixels.
#[cfg(feature = "store")]
const EMBED_WIDTH: u32 = 400;
/// Largest an own cover is kept: it goes out with every change of its book.
#[cfg(feature = "store")]
const EMBED_BYTES: usize = 80 * 1024;

/// The cover in the image file at `path` as a small JPEG `data:` URL.
#[cfg(feature = "store")]
pub fn embed(path: &std::path::Path) -> anyhow::Result<String> {
    use anyhow::Context;
    use base64::Engine;
    let image = image::ImageReader::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .with_guessed_format()?
        .decode()
        .with_context(|| format!("{} is not a JPEG or PNG image", path.display()))?;
    let image = if image.width() > EMBED_WIDTH { image.resize(EMBED_WIDTH, u32::MAX, image::imageops::FilterType::Lanczos3) } else { image };
    let rgb = image.to_rgb8();
    let mut quality = 85;
    loop {
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, quality).encode_image(&rgb)?;
        if jpeg.len() <= EMBED_BYTES || quality <= 40 {
            return Ok(format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(jpeg)));
        }
        quality -= 15;
    }
}

/// The media type of a `data:` URL.
fn data_mime(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("data:")?;
    Some(rest.split([';', ',']).next().unwrap_or_default())
}

#[cfg(feature = "store")]
fn decode_data(url: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine;
    let (head, data) = url.split_once(',').ok_or_else(|| anyhow::anyhow!("broken data: URL"))?;
    anyhow::ensure!(head.ends_with(";base64"), "data: URL isn't base64");
    Ok(base64::engine::general_purpose::STANDARD.decode(data.trim())?)
}

/// Whether `url` is a local file (not synced; [`embed`] makes it one that is).
pub fn is_local(url: &str) -> bool {
    local_path(url).is_some()
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
    fn own_images_are_embedded_small_and_written_to_the_cache() {
        let dir = std::env::temp_dir().join(format!("habit-books-covers-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("photo.png");
        image::RgbImage::from_fn(1200, 1800, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x ^ y) % 256) as u8]))
            .save(&photo)
            .unwrap();
        let url = embed(&photo).unwrap();
        assert!(url.starts_with("data:image/jpeg;base64,"));
        assert!(url.len() < EMBED_BYTES * 4 / 3 + 64, "{} bytes", url.len());
        assert!(path(&url).extension().is_some_and(|e| e == "jpg"));

        // Written out once, from the URL itself.
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let cached = fetch(&url).unwrap();
        let back = image::open(&cached).unwrap();
        assert_eq!((back.width(), back.height()), (EMBED_WIDTH, 600));
        assert!(embed(&dir.join("nope.png")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn data_urls_are_told_apart() {
        assert_eq!(data_mime("data:image/png;base64,AAAA"), Some("image/png"));
        assert_eq!(data_mime("https://x/y.png"), None);
        assert!(path("data:image/png;base64,AAAA").extension().is_some_and(|e| e == "png"));
        assert!(is_local("~/covers/a.jpg") && !is_local("data:image/png;base64,AAAA"));
    }

    #[cfg(feature = "store")]
    #[test]
    fn missing_local_file_is_an_error() {
        assert!(fetch("/definitely/not/here.png").is_err());
    }
}

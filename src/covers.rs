//! Artwork fetched from the catalog, cached on disk.

use futures::StreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub const MAX_BYTES: usize = 8 * 1024 * 1024;

pub struct Covers {
    cache_dir: PathBuf,
    pub http: reqwest::Client,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Covers {
    pub fn new(cache_dir: &Path) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("http client");
        Self {
            cache_dir: cache_dir.to_path_buf(),
            http,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn path(&self, url: &str) -> PathBuf {
        let digest = hex::encode(Sha256::digest(url.as_bytes()));
        self.cache_dir
            .join(&digest[..2])
            .join(format!("{digest}.img"))
    }

    /// Image bytes and content type, or None when unavailable.
    pub async fn get(&self, url: &str) -> Option<(Vec<u8>, &'static str)> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return None;
        }
        let path = self.path(url);
        let lock = self
            .locks
            .lock()
            .entry(url.to_string())
            .or_default()
            .clone();
        let result = {
            let _guard = lock.lock().await;
            self.get_locked(url, &path).await
        };
        let mut locks = self.locks.lock();
        if Arc::strong_count(&lock) <= 2 {
            locks.remove(url);
        }
        result
    }

    async fn get_locked(&self, url: &str, path: &Path) -> Option<(Vec<u8>, &'static str)> {
        if let Ok(meta) = tokio::fs::metadata(path).await {
            // An oversized or truncated entry (cached by an older version) is fetched again.
            if meta.len() as usize <= MAX_BYTES
                && let Ok(data) = tokio::fs::read(path).await
                && is_complete_image(&data)
            {
                let ctype = content_type(&data);
                return Some((data, ctype));
            }
            let _ = tokio::fs::remove_file(path).await;
        }
        let data = match self.download(url).await {
            Ok(Some(d)) => d,
            Ok(None) => return None,
            Err(e) => {
                tracing::debug!("cover {url} unavailable: {e}");
                return None;
            }
        };
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let tmp = path.with_extension("tmp");
        if tokio::fs::write(&tmp, &data).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, path).await;
        }
        let ctype = content_type(&data);
        Some((data, ctype))
    }

    async fn download(&self, url: &str) -> reqwest::Result<Option<Vec<u8>>> {
        let resp = self.http.get(url).send().await?;
        if resp.status() != reqwest::StatusCode::OK {
            return Ok(None);
        }
        let expected = resp.content_length();
        let mut stream = resp.bytes_stream();
        let mut data = Vec::new();
        while let Some(chunk) = stream.next().await {
            data.extend_from_slice(&chunk?);
            if data.len() > MAX_BYTES {
                return Ok(None);
            }
        }
        if expected.is_some_and(|n| n as usize != data.len()) {
            return Ok(None);
        }
        if !is_complete_image(&data) {
            tracing::warn!("cover {url} is not a complete image ({} bytes)", data.len());
            return Ok(None);
        }
        Ok(Some(data))
    }
}

pub fn is_webp(data: &[u8]) -> bool {
    data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP"
}

/// True when the bytes hold a whole JPEG/PNG/WebP, not the first part of one.
pub fn is_complete_image(data: &[u8]) -> bool {
    let tail = |n: usize| &data[data.len().saturating_sub(n)..];
    let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    if data.starts_with(b"\xff\xd8") {
        return contains(tail(64), b"\xff\xd9");
    }
    if data.starts_with(b"\x89PNG") {
        return contains(tail(16), b"IEND");
    }
    if is_webp(data) {
        let declared = u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize;
        return data.len() >= 8 + declared;
    }
    false
}

pub fn content_type(data: &[u8]) -> &'static str {
    if data.starts_with(b"\x89PNG") {
        "image/png"
    } else if is_webp(data) {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::serve;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Path as UrlPath;
    use axum::response::Response;

    fn jpeg() -> Vec<u8> {
        let mut v = b"\xff\xd8\xff\xe0".to_vec();
        for _ in 0..400 {
            v.extend(0u8..=255);
        }
        v.extend(b"\xff\xd9");
        v
    }

    async fn cdn() -> crate::testutil::Server {
        let router = Router::new().route(
            "/{name}",
            axum::routing::get(|UrlPath(name): UrlPath<String>| async move {
                let full = jpeg();
                let body = if name == "cut.jpg" {
                    full[..full.len() - 100].to_vec()
                } else {
                    full.clone()
                };
                // Like a CDN: the body arrives in several chunks, with the full length announced.
                let chunks: Vec<Result<Vec<u8>, std::io::Error>> =
                    body.chunks(8192).map(|c| Ok(c.to_vec())).collect();
                Response::builder()
                    .header("Content-Type", "image/jpeg")
                    .header("Content-Length", full.len())
                    .body(Body::from_stream(futures::stream::iter(chunks)))
                    .unwrap()
            }),
        );
        serve(router).await
    }

    #[tokio::test]
    async fn reads_the_whole_image_and_caches_it() {
        let dir = tempfile::tempdir().unwrap();
        let server = cdn().await;
        let covers = Covers::new(dir.path());
        let url = format!("{}/a.jpg", server.url);
        let (data, ctype) = covers.get(&url).await.unwrap();
        assert_eq!((data, ctype), (jpeg(), "image/jpeg"));
        drop(server);
        assert_eq!(covers.get(&url).await.unwrap().0, jpeg(), "from disk");
    }

    #[tokio::test]
    async fn short_body_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let server = cdn().await;
        let covers = Covers::new(dir.path());
        assert!(
            covers
                .get(&format!("{}/cut.jpg", server.url))
                .await
                .is_none()
        );
        assert!(walk(dir.path()).is_empty());
    }

    fn walk(p: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.path());
            }
        }
        out
    }

    #[tokio::test]
    async fn truncated_cache_entry_is_downloaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let server = cdn().await;
        let covers = Covers::new(dir.path());
        let url = format!("{}/b.jpg", server.url);
        let path = covers.path(&url);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &jpeg()[..1378]).unwrap();
        assert_eq!(covers.get(&url).await.unwrap().0, jpeg());
        assert_eq!(std::fs::read(&path).unwrap(), jpeg());
        assert!(covers.get("file:///etc/passwd").await.is_none());
    }

    #[test]
    fn complete_images() {
        let j = jpeg();
        assert!(is_complete_image(&j));
        assert!(!is_complete_image(&j[..1378]));
        assert!(!is_complete_image(b""));
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend([b'x'; 100]);
        png.extend(b"\x00\x00\x00\x00IEND\xaeB`\x82");
        assert!(is_complete_image(&png) && !is_complete_image(&png[..50]));
        let mut webp = b"RIFF\x08\x00\x00\x00WEBPabcd".to_vec();
        assert!(is_complete_image(&webp));
        assert_eq!(content_type(&webp), "image/webp");
        webp.truncate(14);
        assert!(!is_complete_image(&webp));
    }
}

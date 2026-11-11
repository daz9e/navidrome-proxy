//! Lyrics from lrclib.net, stored next to the track where Navidrome picks them up.

use crate::util::{Json, as_f64, as_str, opt_str};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const LRCLIB: &str = "https://lrclib.net/api/get";

/// `("lrc" | "txt", text)` or None.
pub async fn fetch(http: &reqwest::Client, meta: &Json, base_url: &str) -> Option<(&'static str, String)> {
    let artist = match meta.get("artists") {
        Some(Value::Array(a)) if !a.is_empty() => as_str(a.first()),
        _ => as_str(meta.get("artist")),
    };
    let duration = (as_f64(meta.get("duration")).unwrap_or(0.0) as i64).to_string();
    let query = [
        ("artist_name", artist),
        ("track_name", as_str(meta.get("title"))),
        ("album_name", as_str(meta.get("album"))),
        ("duration", duration),
    ];
    let response = http
        .get(base_url)
        .query(&query)
        .timeout(Duration::from_secs(15))
        .header("User-Agent", "navidrome-proxy/3 (https://github.com/daz9e/navidrome-proxy)")
        .send()
        .await;
    let data: Value = match response {
        Ok(r) if r.status() == reqwest::StatusCode::OK => r.json().await.ok()?,
        Ok(_) => return None,
        Err(e) => {
            tracing::debug!("lyrics lookup failed for {}: {e}", as_str(meta.get("title")));
            return None;
        }
    };
    let data = data.as_object()?;
    if crate::util::truthy(data.get("instrumental")) {
        return None;
    }
    if let Some(text) = opt_str(data.get("syncedLyrics")) {
        return Some(("lrc", text));
    }
    opt_str(data.get("plainLyrics")).map(|t| ("txt", t))
}

pub fn save(audio_path: &Path, kind: &str, text: &str) -> std::io::Result<PathBuf> {
    let target = audio_path.with_extension(kind);
    std::fs::write(&target, text)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{m, serve};
    use axum::Router;
    use serde_json::json;

    #[tokio::test]
    async fn prefers_synced_lyrics_and_skips_instrumentals() {
        let router = Router::new().route(
            "/get",
            axum::routing::get(|q: axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                axum::Json(match q.get("track_name").map(String::as_str) {
                    Some("Synced") => json!({"syncedLyrics": "[00:01.00] la", "plainLyrics": "la"}),
                    Some("Plain") => json!({"plainLyrics": "la", "syncedLyrics": null}),
                    Some("Instr") => json!({"instrumental": true, "plainLyrics": "x"}),
                    _ => json!({"code": 404}),
                })
            }),
        );
        let server = serve(router).await;
        let url = format!("{}/get", server.url);
        let http = reqwest::Client::new();
        let meta = |t: &str| m(json!({"title": t, "artists": ["A"], "duration": 200.7}));
        assert_eq!(fetch(&http, &meta("Synced"), &url).await, Some(("lrc", "[00:01.00] la".into())));
        assert_eq!(fetch(&http, &meta("Plain"), &url).await, Some(("txt", "la".into())));
        assert_eq!(fetch(&http, &meta("Instr"), &url).await, None);
        assert_eq!(fetch(&http, &meta("Nope"), &url).await, None);

        let dir = tempfile::tempdir().unwrap();
        let audio = dir.path().join("01 - Song.m4a");
        let saved = save(&audio, "lrc", "x").unwrap();
        assert_eq!(saved, dir.path().join("01 - Song.lrc"));
    }
}

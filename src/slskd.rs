//! Quality upgrades from Soulseek (slskd) for liked tracks.
//!
//! A liked track is first downloaded from YouTube Music (fast, AAC ~128 kbit/s). A
//! background job then looks for a better lossy copy on Soulseek and replaces the
//! file only after it proves to be the *same recording*: its Chromaprint
//! fingerprint must match the file we already have. Peers' file names and tags are
//! not trusted — that is how a "minus" (instrumental) once replaced FORTUNA 812 —
//! Black Water in the first proxy generation.

use crate::audio::{self, Tools};
use crate::covers::Covers;
use crate::db::{Field, Store, Track};
use crate::library::Library;
use crate::matching::{self, Wanted};
use crate::reconcile::known_duration;
use crate::scheduler::{Handler, JobResult};
use crate::sources::SourceError;
use crate::util::{as_f64, as_str, as_uint, opt_str, truthy};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

pub const SIMILARITY_THRESHOLD: f64 = 0.88;

/// Accepted lossy suffixes and their minimum bitrate (kbit/s).
fn min_bitrate(suffix: &str) -> Option<i64> {
    matches!(suffix, "mp3" | "m4a" | "aac").then_some(256)
}

/// Python's `quote(s, safe="")`: peer names may contain "/", "?" or "#".
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// Components of a peer's (Windows-style) path.
fn parts(filename: &str) -> Vec<&str> {
    filename.split(['\\', '/']).filter(|p| !p.is_empty()).collect()
}

fn split_suffix(name: &str) -> (&str, String) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], name[i + 1..].to_lowercase()),
        _ => (name, String::new()),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteFile {
    pub username: String,
    pub filename: String,
    pub size: u64,
    pub bitrate: i64,
    pub length: Option<f64>,
    pub free_slot: bool,
    pub queue: i64,
    pub peers: usize,
    pub score: f64,
}

impl RemoteFile {
    pub fn new(username: &str, filename: &str, size: u64) -> Self {
        Self { username: username.into(), filename: filename.into(), size, bitrate: 320, length: None, free_slot: true, queue: 0, peers: 1, score: 0.0 }
    }

    pub fn basename(&self) -> &str {
        parts(&self.filename).last().copied().unwrap_or("")
    }

    fn folder(&self) -> &str {
        let p = parts(&self.filename);
        if p.len() >= 2 { p[p.len() - 2] } else { "" }
    }
}

/// Acceptable files, best first; identical copies (name+size) are counted as peers.
pub fn select(responses: &[Value], wanted: &Wanted, min_peers: usize, tolerance: f64) -> Vec<RemoteFile> {
    let mut found = Vec::new();
    for response in responses {
        let username = as_str(response.get("username"));
        for f in response.get("files").and_then(Value::as_array).into_iter().flatten() {
            let name = as_str(f.get("filename"));
            let p = parts(&name);
            let (stem, suffix) = split_suffix(p.last().copied().unwrap_or(""));
            let Some(min) = min_bitrate(&suffix) else { continue };
            if truthy(f.get("isLocked")) {
                continue;
            }
            let bitrate = as_uint(f.get("bitRate"));
            if bitrate != 0 && bitrate < min {
                continue;
            }
            let length = as_f64(f.get("length")).filter(|l| *l != 0.0);
            let folder = p[p.len().saturating_sub(3)..p.len().saturating_sub(1)].join(" ");
            let scored = matching::score_source_candidate(wanted, stem, &folder, length, false, &[], tolerance);
            if scored.rejected.is_some() || scored.score < 55.0 || (wanted.duration.is_some_and(|d| d != 0.0) && length.is_none()) {
                continue;
            }
            found.push(RemoteFile {
                username: username.clone(),
                filename: name.clone(),
                size: as_uint(f.get("size")) as u64,
                bitrate,
                length,
                free_slot: truthy(response.get("hasFreeUploadSlot")),
                queue: as_uint(response.get("queueLength")),
                peers: 1,
                score: scored.score,
            });
        }
    }
    let key = |rf: &RemoteFile| (rf.basename().to_lowercase(), rf.size);
    let keys: Vec<_> = found.iter().map(key).collect();
    for rf in &mut found {
        let k = key(rf);
        rf.peers = keys.iter().filter(|x| **x == k).count();
    }
    found.retain(|rf| rf.peers >= min_peers);
    found.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.bitrate.min(320).cmp(&a.bitrate.min(320)))
            .then(b.peers.cmp(&a.peers))
            .then(b.free_slot.cmp(&a.free_slot))
            .then(a.queue.cmp(&b.queue))
    });
    found
}

pub struct Slskd {
    url: String,
    downloads_dir: PathBuf,
    http: reqwest::Client,
    pub poll: Duration,
    pub search_timeout: Duration,
    pub download_timeout: Duration,
}

impl Slskd {
    pub fn new(url: &str, api_key: &str, downloads_dir: &Path) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(v) = reqwest::header::HeaderValue::from_str(api_key) {
            headers.insert("X-API-Key", v);
        }
        let http = reqwest::Client::builder().default_headers(headers).timeout(Duration::from_secs(30)).build().expect("http client");
        Self {
            url: url.trim_end_matches('/').to_string(),
            downloads_dir: downloads_dir.to_path_buf(),
            http,
            poll: Duration::from_secs(2),
            search_timeout: Duration::from_secs(45),
            download_timeout: Duration::from_secs(600),
        }
    }

    async fn json(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, SourceError> {
        let mut req = self.http.request(method.clone(), format!("{}/api/v0/{path}", self.url));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.map_err(|e| SourceError::new(format!("slskd unavailable: {e}"), true))?;
        let status = resp.status();
        if status.as_u16() >= 400 {
            return Err(SourceError::new(format!("slskd {method} {path}: HTTP {}", status.as_u16()), true));
        }
        let bytes = resp.bytes().await.map_err(|e| SourceError::new(format!("slskd unavailable: {e}"), true))?;
        if status.as_u16() == 204 || bytes.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes).map_err(|e| SourceError::new(format!("slskd {method} {path}: bad JSON: {e}"), true))
    }

    /// Search responses; the search is always deleted afterwards.
    pub async fn search(&self, text: &str) -> Result<Vec<Value>, SourceError> {
        let created = self
            .json(
                reqwest::Method::POST,
                "searches",
                Some(json!({
                    "searchText": text, "searchTimeout": self.search_timeout.as_millis() as u64,
                    "responseLimit": 200, "filterResponses": true,
                })),
            )
            .await?;
        // B2: a body without an id is an slskd failure, not a crash.
        let id = opt_str(created.get("id")).ok_or_else(|| SourceError::new("slskd search: no id in response", true))?;
        let id = utf8_percent_encode(&id, SEGMENT).to_string();
        let result = self.collect(&id).await;
        let _ = self.json(reqwest::Method::DELETE, &format!("searches/{id}"), None).await;
        result
    }

    async fn collect(&self, id: &str) -> Result<Vec<Value>, SourceError> {
        let deadline = Instant::now() + self.search_timeout + Duration::from_secs(10);
        while Instant::now() < deadline {
            let state = self.json(reqwest::Method::GET, &format!("searches/{id}"), None).await?;
            if truthy(state.get("isComplete")) {
                break;
            }
            tokio::time::sleep(self.poll).await;
        }
        let found = self.json(reqwest::Method::GET, &format!("searches/{id}/responses"), None).await?;
        Ok(found.as_array().cloned().unwrap_or_default())
    }

    pub async fn download(&self, rf: &RemoteFile) -> Result<PathBuf, SourceError> {
        let user_path = format!("transfers/downloads/{}", utf8_percent_encode(&rf.username, SEGMENT));
        self.json(reqwest::Method::POST, &user_path, Some(json!([{"filename": rf.filename, "size": rf.size}]))).await?;
        let deadline = Instant::now() + self.download_timeout;
        while Instant::now() < deadline {
            tokio::time::sleep(self.poll).await;
            let user = self.json(reqwest::Method::GET, &user_path, None).await?;
            let files = user.get("directories").and_then(Value::as_array).into_iter().flatten().flat_map(|d| d.get("files").and_then(Value::as_array).into_iter().flatten());
            for f in files {
                if f.get("filename").and_then(Value::as_str) != Some(rf.filename.as_str()) {
                    continue;
                }
                let state = as_str(f.get("state"));
                if state.contains("Succeeded") {
                    if let Some(path) = self.locate(rf) {
                        return Ok(path);
                    }
                } else if state.contains("Completed") {
                    // B3: only a refusal or an error is final; cancelled/timed out may work later.
                    let permanent = state.contains("Errored") || state.contains("Rejected");
                    return Err(SourceError::new(format!("slskd transfer {state}"), !permanent));
                }
            }
        }
        Err(SourceError::new("slskd transfer timed out", true))
    }

    /// slskd stores `<downloads>/<remote folder>/<file name>`; match by name and size.
    pub fn locate(&self, rf: &RemoteFile) -> Option<PathBuf> {
        let name = rf.basename();
        if name.is_empty() || name == "." || name == ".." {
            return None;
        }
        let fits = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == rf.size);
        let folder = rf.folder();
        if !matches!(folder, "" | "." | "..") {
            let direct = self.downloads_dir.join(folder).join(name);
            if fits(&direct) {
                return Some(direct);
            }
        }
        let mut stack = vec![self.downloads_dir.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(path);
                } else if entry.file_name().to_str() == Some(name) && fits(&path) {
                    return Some(path);
                }
            }
        }
        None
    }
}

pub struct Upgrader {
    pub store: Arc<Store>,
    pub library: Arc<Library>,
    pub client: Arc<Slskd>,
    pub min_peers: usize,
    pub covers: Option<Arc<Covers>>,
    pub tools: Tools,
}

fn io_error(e: impl std::fmt::Display) -> SourceError {
    SourceError::new(e.to_string(), true)
}

impl Upgrader {
    /// The scheduler handler for jobs of kind "upgrade".
    pub fn handler(self: &Arc<Self>) -> Handler {
        let me = self.clone();
        Arc::new(move |job, _cancel| {
            let me = me.clone();
            Box::pin(async move { me.upgrade(&job.track_id).await })
        })
    }

    pub async fn upgrade(&self, track_id: &str) -> JobResult {
        let Some(track) = self.store.get_track(track_id).filter(Track::is_ready) else { return Ok(None) };
        if as_uint(track.audio.get("bitrate")) >= 256 || track.source.as_deref() == Some("soulseek") {
            return Ok(Some(track));
        }
        let artists = track.artists();
        let wanted = Wanted { title: track.title(), artists: artists.clone(), duration: Some(known_duration(&track)), ..Default::default() };
        let responses = self.client.search(&format!("{} {}", artists.first().map(String::as_str).unwrap_or(""), track.title())).await?;
        let candidates = select(&responses, &wanted, self.min_peers, 3.0);
        if candidates.is_empty() {
            tracing::info!("no Soulseek upgrade for {} ({} responses)", track.id, responses.len());
            return Ok(Some(track));
        }
        let file = PathBuf::from(track.file.clone().unwrap_or_default());
        let Some(reference) = self.tools.fingerprint(&file, 120).await else {
            return Err(SourceError::new("fpcalc unavailable: refusing an unverifiable upgrade", false));
        };
        for rf in candidates.iter().take(3) {
            match self.attempt(&track, rf, &reference).await {
                Ok(upgraded) => return Ok(Some(upgraded)),
                Err(e) => tracing::info!("Soulseek candidate {} from {} rejected: {e}", rf.basename(), rf.username),
            }
        }
        Ok(Some(track))
    }

    async fn attempt(&self, track: &Track, rf: &RemoteFile, reference: &[u32]) -> Result<Track, SourceError> {
        let raw = self.client.download(rf).await?;
        let staging = self.library.new_staging(&track.id).map_err(io_error);
        let result = match &staging {
            Ok(dir) => self.replace(track, rf, reference, &raw, dir).await,
            Err(e) => Err(e.clone()),
        };
        if let Ok(dir) = staging {
            let _ = tokio::fs::remove_dir_all(dir).await;
        }
        let _ = tokio::fs::remove_file(&raw).await;
        result
    }

    async fn replace(&self, track: &Track, rf: &RemoteFile, reference: &[u32], raw: &Path, staging: &Path) -> Result<Track, SourceError> {
        let copy = staging.join(format!("source.{}", audio::suffix_of(raw)));
        tokio::fs::copy(raw, &copy).await.map_err(io_error)?;
        let (prepared, info) = self.tools.normalize(&copy, staging, "prepared").await.map_err(|e| SourceError::new(e.0, false))?;
        let current = known_duration(track);
        if current != 0.0 && (info.duration - current).abs() > 3.0 {
            return Err(SourceError::new(format!("duration {:.0}s vs {current:.0}s", info.duration), false));
        }
        let candidate = self.tools.fingerprint(&prepared, 120).await.unwrap_or_default();
        let similarity = audio::fingerprint_similarity(reference, &candidate, 80);
        if similarity < SIMILARITY_THRESHOLD {
            return Err(SourceError::new(format!("different recording (similarity {similarity:.2})"), false));
        }
        let cover = match (&self.covers, opt_str(track.meta.get("cover"))) {
            (Some(covers), Some(url)) => crate::pipeline::cover_for_tags(covers, &self.tools, &url, staging).await,
            _ => None,
        };
        let (path, meta) = (prepared.clone(), track.meta.clone());
        tokio::task::spawn_blocking(move || crate::tagging::write_tags(&path, &meta, cover.as_deref()))
            .await
            .map_err(io_error)?
            .map_err(|e| SourceError::new(format!("tagging failed: {e}"), false))?;
        let (library, id, meta, previous) = (self.library.clone(), track.id.clone(), track.meta.clone(), track.file.clone());
        let dest = tokio::task::spawn_blocking(move || library.publish(&id, &prepared, &meta, previous.as_deref())).await.map_err(io_error)?.map_err(io_error)?;
        let mut audio = info.as_json();
        audio.insert("size".into(), json!(std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0)));
        self.store.update_track(
            &track.id,
            &[
                Field::File(Some(dest.display().to_string())),
                Field::Source(Some("soulseek".into())),
                Field::SourceRef(Some(rf.basename().to_string())),
                Field::Audio(Some(audio)),
            ],
        );
        tracing::info!("upgraded {} to {} kbit/s from Soulseek (similarity {similarity:.2})", track.id, info.bitrate);
        self.store.get_track(&track.id).ok_or_else(|| SourceError::new("track vanished", true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{have, make_music, need};
    use crate::testutil::{m, serve};
    use axum::Router;
    use axum::extract::{Path as UrlPath, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use parking_lot::Mutex;
    use std::collections::HashMap;

    fn response(user: &str, files: Vec<Value>) -> Value {
        json!({"username": user, "hasFreeUploadSlot": true, "queueLength": 0, "files": files})
    }

    fn rfile(name: &str, size: u64, bitrate: i64, length: u64) -> Value {
        json!({"filename": name, "size": size, "bitRate": bitrate, "length": length})
    }

    fn wanted() -> Wanted {
        Wanted { duration: Some(40.0), ..Wanted::new("Black Water", &["FORTUNA 812"]) }
    }

    #[test]
    fn select_filters_versions_bitrate_and_counts_peers() {
        let responses = [
            response("a", vec![rfile("music\\FORTUNA 812\\Project n(o)va\\08 Black Water.mp3", 5000, 320, 40)]),
            response("b", vec![rfile("x\\FORTUNA 812\\Project n(o)va\\08 Black Water.mp3", 5000, 320, 40)]),
            response("c", vec![rfile("x\\FORTUNA 812\\08 Black Water (минус).mp3", 5000, 320, 40)]),
            response("d", vec![rfile("x\\FORTUNA 812\\08 Black Water.mp3", 3000, 128, 40)]),
            response("e", vec![rfile("x\\FORTUNA 812\\08 Black Water.flac", 9000, 1000, 40)]),
            response("f", vec![rfile("x\\FORTUNA 812\\08 Black Water.mp3", 4000, 320, 90)]),
            response("g", vec![json!({"filename": "x\\FORTUNA 812\\08 Black Water.mp3", "size": 5000, "isLocked": true, "length": 40})]),
        ];
        let picked = select(&responses, &wanted(), 2, 3.0);
        let got: Vec<_> = picked.iter().map(|r| (r.username.as_str(), r.peers)).collect();
        assert_eq!(got, [("a", 2), ("b", 2)]);
        assert!(select(&responses, &wanted(), 3, 3.0).is_empty());
    }

    #[test]
    fn select_orders_by_bitrate_then_free_slot() {
        let mut slow = response("slow", vec![rfile("x\\FORTUNA 812\\Black Water.mp3", 1, 320, 40)]);
        slow["hasFreeUploadSlot"] = json!(false);
        let responses = [
            response("low", vec![rfile("x\\FORTUNA 812\\Black Water.mp3", 2, 256, 40)]),
            slow,
            response("fast", vec![rfile("x\\FORTUNA 812\\Black Water.mp3", 3, 320, 40)]),
            response("nolength", vec![json!({"filename": "x\\FORTUNA 812\\Black Water.mp3", "size": 4, "bitRate": 320})]),
        ];
        let order: Vec<_> = select(&responses, &wanted(), 1, 3.0).into_iter().map(|r| r.username).collect();
        assert_eq!(order, ["fast", "slow", "low"]);
    }

    #[test]
    fn locate_finds_names_with_brackets_outside_the_remote_folder() {
        let dir = tempfile::tempdir().unwrap();
        let stored = dir.path().join("other folder").join("[2019] Black Water.mp3");
        std::fs::create_dir_all(stored.parent().unwrap()).unwrap();
        std::fs::write(&stored, b"xxxxxxxxxx").unwrap();
        let client = Slskd::new("http://unused", "key", dir.path());
        assert_eq!(client.locate(&RemoteFile::new("peer", "music\\Album\\[2019] Black Water.mp3", 10)), Some(stored));
        assert_eq!(client.locate(&RemoteFile::new("peer", "music\\Album\\[2019] Black Water.mp3", 11)), None, "size must match");
    }

    #[test]
    fn locate_ignores_a_parent_folder_from_the_peer() {
        let dir = tempfile::tempdir().unwrap();
        let inside = dir.path().join("dl");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(dir.path().join("Song.mp3"), b"x").unwrap();
        let client = Slskd::new("http://unused", "key", &inside);
        assert_eq!(client.locate(&RemoteFile::new("peer", "a\\..\\Song.mp3", 1)), None);
    }

    /// A scripted slskd: search completes on the n-th poll; transfers copy `files` into `downloads`.
    #[derive(Default)]
    struct Fake {
        downloads: PathBuf,
        files: Vec<(String, PathBuf)>,
        polls_until_complete: usize,
        search_status: u16,
        create_body: Option<Value>,
        transfer_state: Option<String>,
        polls: usize,
        deleted: Vec<String>,
        keys: Vec<String>,
        searched: Vec<String>,
        transfers: HashMap<String, Vec<Value>>,
    }

    type S = Arc<Mutex<Fake>>;

    fn router(fake: S) -> Router {
        async fn create(State(s): State<S>, headers: HeaderMap, axum::Json(body): axum::Json<Value>) -> axum::Json<Value> {
            let mut f = s.lock();
            f.keys.push(headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("").into());
            f.searched.push(as_str(body.get("searchText")));
            axum::Json(f.create_body.clone().unwrap_or(json!({"id": "s1"})))
        }
        async fn state(State(s): State<S>) -> Result<axum::Json<Value>, StatusCode> {
            let mut f = s.lock();
            f.polls += 1;
            if f.search_status != 0 {
                return Err(StatusCode::from_u16(f.search_status).unwrap());
            }
            Ok(axum::Json(json!({"isComplete": f.polls >= f.polls_until_complete})))
        }
        async fn responses(State(s): State<S>) -> axum::Json<Value> {
            let f = s.lock();
            let list: Vec<Value> = f
                .files
                .iter()
                .enumerate()
                .map(|(i, (name, path))| response(&format!("user{i}"), vec![rfile(name, std::fs::metadata(path).unwrap().len(), 320, 40)]))
                .collect();
            axum::Json(Value::Array(list))
        }
        async fn delete(State(s): State<S>, UrlPath(id): UrlPath<String>) -> StatusCode {
            s.lock().deleted.push(id);
            StatusCode::NO_CONTENT
        }
        async fn enqueue(State(s): State<S>, UrlPath(user): UrlPath<String>, axum::Json(items): axum::Json<Vec<Value>>) -> StatusCode {
            let mut f = s.lock();
            for item in items {
                let name = as_str(item.get("filename"));
                let state = match &f.transfer_state {
                    Some(st) => st.clone(),
                    None => {
                        let src = f.files.iter().find(|(n, _)| *n == name).unwrap().1.clone();
                        let p = parts(&name);
                        let dest = f.downloads.join(p[p.len() - 2]).join(p[p.len() - 1]);
                        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
                        std::fs::copy(src, dest).unwrap();
                        "Completed, Succeeded".into()
                    }
                };
                f.transfers.entry(user.clone()).or_default().push(json!({"filename": name, "state": state}));
            }
            StatusCode::CREATED
        }
        async fn status(State(s): State<S>, UrlPath(user): UrlPath<String>) -> axum::Json<Value> {
            let files = s.lock().transfers.get(&user).cloned().unwrap_or_default();
            axum::Json(json!({"directories": [{"files": files}]}))
        }
        Router::new()
            .route("/api/v0/searches", post(create))
            .route("/api/v0/searches/{id}", get(state).delete(delete))
            .route("/api/v0/searches/{id}/responses", get(responses))
            .route("/api/v0/transfers/downloads/{user}", post(enqueue).get(status))
            .with_state(fake)
    }

    fn fast(client: Slskd) -> Slskd {
        Slskd { poll: Duration::from_millis(5), search_timeout: Duration::from_secs(5), download_timeout: Duration::from_secs(5), ..client }
    }

    #[tokio::test]
    async fn search_polls_until_complete_and_always_deletes_the_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mp3"), b"x").unwrap();
        let fake = Arc::new(Mutex::new(Fake { polls_until_complete: 3, files: vec![("x\\A\\Black Water.mp3".into(), dir.path().join("a.mp3"))], ..Default::default() }));
        let server = serve(router(fake.clone())).await;
        let client = fast(Slskd::new(&server.url, "secret-key", dir.path()));
        let found = client.search("FORTUNA 812 Black Water").await.unwrap();
        assert_eq!(found[0]["username"], "user0");
        let f = fake.lock();
        assert_eq!((f.polls, f.deleted.clone(), f.keys.clone()), (3, vec!["s1".to_string()], vec!["secret-key".to_string()]));
        drop(f);

        let broken = Arc::new(Mutex::new(Fake { search_status: 500, ..Default::default() }));
        let server = serve(router(broken.clone())).await;
        let err = fast(Slskd::new(&server.url, "k", dir.path())).search("x").await.unwrap_err();
        assert!(err.to_string().contains("HTTP 500") && err.transient());
        assert_eq!(broken.lock().deleted, ["s1"]);
    }

    #[tokio::test]
    async fn search_without_an_id_is_a_transient_error() {
        let fake = Arc::new(Mutex::new(Fake { create_body: Some(json!({"state": "?"})), ..Default::default() }));
        let server = serve(router(fake)).await;
        let err = fast(Slskd::new(&server.url, "k", Path::new("/nonexistent"))).search("x").await.unwrap_err();
        assert!(err.transient() && err.to_string().contains("no id"));
    }

    #[tokio::test]
    async fn download_escapes_peer_names() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src.mp3");
        std::fs::write(&source, b"yyyyyyyyyy").unwrap();
        let dl = dir.path().join("dl");
        let fake = Arc::new(Mutex::new(Fake { downloads: dl.clone(), files: vec![("a\\Album\\Song.mp3".into(), source)], ..Default::default() }));
        let server = serve(router(fake.clone())).await;
        let client = fast(Slskd::new(&server.url, "k", &dl));
        let path = client.download(&RemoteFile::new("dj/one?#", "a\\Album\\Song.mp3", 10)).await.unwrap();
        assert_eq!(path, dl.join("Album").join("Song.mp3"));
        assert_eq!(fake.lock().transfers.keys().collect::<Vec<_>>(), ["dj/one?#"]);
    }

    #[tokio::test]
    async fn failed_transfers_and_stuck_ones() {
        let rf = RemoteFile::new("peer", "x\\A\\Song.mp3", 10);
        for (state, transient) in [("Completed, Errored", false), ("Completed, Rejected", false), ("Completed, Cancelled", true)] {
            let fake = Arc::new(Mutex::new(Fake { transfer_state: Some(state.into()), ..Default::default() }));
            let server = serve(router(fake)).await;
            let err = fast(Slskd::new(&server.url, "k", Path::new("/nonexistent"))).download(&rf).await.unwrap_err();
            assert_eq!(err.transient(), transient, "{state}");
        }
        let fake = Arc::new(Mutex::new(Fake { transfer_state: Some("InProgress".into()), ..Default::default() }));
        let server = serve(router(fake)).await;
        let client = Slskd { download_timeout: Duration::from_millis(50), ..fast(Slskd::new(&server.url, "k", Path::new("/nonexistent"))) };
        let err = client.download(&rf).await.unwrap_err();
        assert!(err.transient() && err.to_string().contains("timed out"));
    }

    struct World {
        dir: tempfile::TempDir,
        store: Arc<Store>,
        fake: S,
        _server: crate::testutil::Server,
        upgrader: Upgrader,
    }

    async fn world(files: Vec<(&str, PathBuf)>, dir: tempfile::TempDir, min_peers: usize) -> World {
        let music = dir.path().join("music");
        let dl = dir.path().join("slskd");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::create_dir_all(&dl).unwrap();
        let fake = Arc::new(Mutex::new(Fake { downloads: dl.clone(), files: files.into_iter().map(|(n, p)| (n.to_string(), p)).collect(), ..Default::default() }));
        let server = serve(router(fake.clone())).await;
        let store = Arc::new(Store::memory());
        let upgrader = Upgrader {
            store: store.clone(),
            library: Arc::new(Library::new(&music, music.join(".staging"))),
            client: Arc::new(fast(Slskd::new(&server.url, "k", &dl))),
            min_peers,
            covers: None,
            tools: Tools::default(),
        };
        World { dir, store, fake, _server: server, upgrader }
    }

    fn ready(store: &Store, file: &Path, bitrate: i64, source: &str) {
        store.remember_track("deezer:1", &m(json!({"title": "Black Water", "artists": ["FORTUNA 812"], "duration": 40, "album": "Project n(o)va"})));
        store.update_track(
            "deezer:1",
            &[
                Field::status("ready"),
                Field::File(Some(file.display().to_string())),
                Field::Source(Some(source.into())),
                Field::Audio(Some(m(json!({"duration": 40, "bitrate": bitrate})))),
            ],
        );
    }

    #[tokio::test]
    async fn upgrader_leaves_good_or_unmatched_tracks_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.m4a");
        std::fs::write(&file, b"x").unwrap();
        let w = world(vec![], dir, 2).await;
        assert_eq!(w.upgrader.upgrade("deezer:1").await.unwrap(), None, "unknown track");
        ready(&w.store, &file, 320, "youtube");
        assert_eq!(w.upgrader.upgrade("deezer:1").await.unwrap().unwrap().source.as_deref(), Some("youtube"));
        assert!(w.fake.lock().searched.is_empty(), "already good enough: no search at all");
        ready(&w.store, &file, 128, "soulseek");
        w.upgrader.upgrade("deezer:1").await.unwrap();
        assert!(w.fake.lock().searched.is_empty(), "already from Soulseek");
        ready(&w.store, &file, 128, "youtube");
        assert_eq!(w.upgrader.upgrade("deezer:1").await.unwrap().unwrap().source.as_deref(), Some("youtube"));
        assert_eq!(w.fake.lock().searched, ["FORTUNA 812 Black Water"], "searched, nothing acceptable");
    }

    #[tokio::test]
    async fn upgrader_refuses_to_replace_without_a_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("peer.mp3");
        std::fs::write(&src, vec![0u8; 5000]).unwrap();
        let file = dir.path().join("t.m4a");
        std::fs::write(&file, b"x").unwrap();
        let mut w = world(vec![("x\\FORTUNA 812\\Black Water.mp3", src)], dir, 1).await;
        w.upgrader.tools.fpcalc = "/nonexistent/fpcalc".into();
        ready(&w.store, &file, 128, "youtube");
        let err = w.upgrader.upgrade("deezer:1").await.unwrap_err();
        assert!(!err.transient() && err.to_string().contains("fpcalc"));
        let _ = &w.dir;
    }

    #[tokio::test]
    async fn upgrade_rejects_other_recording_and_accepts_the_same_one() {
        need!("ffmpeg", "fpcalc");
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        let original = make_music(&music.join("orig.m4a"), 40, 0);
        // "minus": other melody, same name and length. Real: the same audio at 320 kbit/s.
        let minus = make_music(&dir.path().join("minus.m4a"), 40, 1);
        let to_mp3 = |src: &Path, dst: PathBuf| {
            let ok = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-y", "-i"])
                .arg(src)
                .args(["-c:a", "libmp3lame", "-b:a", "320k"])
                .arg(&dst)
                .status()
                .unwrap()
                .success();
            assert!(ok);
            dst
        };
        let minus = to_mp3(&minus, dir.path().join("minus.mp3"));
        let real = to_mp3(&original, dir.path().join("real.mp3"));
        let w = world(vec![("a\\FORTUNA 812\\Black Water.mp3", minus), ("b\\FORTUNA 812 - Project\\Black Water.mp3", real)], dir, 1).await;
        ready(&w.store, &original, 128, "youtube");
        let result = w.upgrader.upgrade("deezer:1").await.unwrap().unwrap();
        assert_eq!((result.source.as_deref(), result.source_ref.as_deref()), (Some("soulseek"), Some("Black Water.mp3")));
        assert!(result.file.as_deref().unwrap().ends_with("[deezer-1].mp3"));
        assert!(as_uint(result.audio.get("bitrate")) >= 300);
        assert_eq!(w.fake.lock().transfers.len(), 2, "the minus was downloaded, checked and refused");
        assert!(!original.exists(), "the old file is replaced");
        for folder in ["FORTUNA 812", "FORTUNA 812 - Project"] {
            assert!(!w.dir.path().join("slskd").join(folder).join("Black Water.mp3").exists(), "raw downloads are removed");
        }
    }
}

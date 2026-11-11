//! Finding and downloading audio for a catalog track.
//!
//! Order of preference:
//!
//! 1. **YouTube Music "songs"** – the official audio uploads (`Artist - Topic`), one
//!    innertube HTTP call (~0.5 s).
//! 2. **YouTube videos** and **SoundCloud** – searched in parallel through yt-dlp only
//!    when YouTube Music has nothing acceptable.
//!
//! Every candidate goes through `matching::score_source_candidate`, which hard
//! rejects wrong durations and unrequested versions (minus/instrumental, karaoke,
//! sped up, remix, live, cover ...).
//!
//! For YouTube, yt-dlp only resolves the stream URL; the file itself is fetched here
//! with a few parallel range requests (0.3 s for a song instead of ~9 s through
//! yt-dlp's own downloader). yt-dlp's downloader stays as the fallback.

use crate::matching::{self, Wanted};
use crate::process::{self, RunError};
use crate::util::{Json, as_f64, as_str, opt_str, path};
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const MIN_SCORE: f64 = 55.0;
pub const YOUTUBE_FORMAT: &str = "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio";
pub const SOUNDCLOUD_FORMAT: &str =
    "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio[ext=mp3]/bestaudio";
const PERMANENT_HINTS: [&str; 9] = [
    "drm",
    "private video",
    "video unavailable",
    "not available",
    "copyright",
    "removed",
    "members-only",
    "sign in to confirm your age",
    "404",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Candidate {
    /// "youtube" | "soundcloud"
    pub source: String,
    pub url: String,
    pub reference: String,
    pub title: String,
    pub uploader: String,
    pub duration: Option<f64>,
    pub official: bool,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub thumbnail: Option<String>,
    pub score: f64,
    pub reasons: Vec<String>,
}

/// Downloading failed; `transient` tells the queue whether a retry makes sense.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SourceError {
    #[error("{message}")]
    Failed { message: String, transient: bool },
    #[error("cancelled")]
    Cancelled,
}

impl SourceError {
    pub fn new(message: impl Into<String>, transient: bool) -> Self {
        Self::Failed {
            message: message.into(),
            transient,
        }
    }

    pub fn transient(&self) -> bool {
        match self {
            Self::Failed { transient, .. } => *transient,
            Self::Cancelled => true,
        }
    }
}

/// A YouTube Music search row, normalized.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct YtmTrack {
    pub video_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration: Option<f64>,
    pub explicit: bool,
    pub thumbnail: Option<String>,
    pub kind: String,
}

/// The network side of `Sources`; tests replace it.
pub trait Backend: Send + Sync {
    fn ytmusic_search(
        &self,
        query: String,
        filter: Option<&'static str>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<YtmTrack>, String>>;
    /// yt-dlp `ytsearchN:` / `scsearchN:` entries.
    fn flat_search(&self, query: String) -> BoxFuture<'_, Result<Vec<Json>, String>>;
    /// The chosen audio format of `url` (direct URL, headers, size), without downloading.
    fn resolve(
        &self,
        url: String,
        format: &'static str,
    ) -> BoxFuture<'_, Result<Json, SourceError>>;
    /// yt-dlp's own downloader into `dest/source.<ext>`.
    fn download(
        &self,
        url: String,
        format: &'static str,
        dest: PathBuf,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> BoxFuture<'_, Result<PathBuf, SourceError>>;
}

pub struct Sources {
    pub tolerance: f64,
    pub backend: Arc<dyn Backend>,
    /// How long a 403 on a just-resolved googlevideo URL is retried: YouTube refuses
    /// such URLs for roughly the first second (measured), then serves them.
    pub fresh_url_grace: Duration,
    http: reqwest::Client,
}

impl Sources {
    pub fn new(tolerance: f64, backend: Arc<dyn Backend>) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("http client");
        Self {
            tolerance,
            backend,
            fresh_url_grace: Duration::from_secs(5),
            http,
        }
    }

    // --------------------------------------------------------------- search
    pub async fn ytmusic(
        &self,
        query: &str,
        filter: Option<&'static str>,
        limit: usize,
    ) -> Vec<YtmTrack> {
        match self
            .backend
            .ytmusic_search(query.to_string(), filter, limit)
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("YouTube Music search failed for {query:?}: {e}");
                vec![]
            }
        }
    }

    pub async fn flat_search(&self, query: &str) -> Vec<Json> {
        match self.backend.flat_search(query.to_string()).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("search failed for {query:?}: {e}");
                vec![]
            }
        }
    }

    fn score(&self, wanted: &Wanted, mut cand: Candidate) -> Option<Candidate> {
        let scored = matching::score_source_candidate(
            wanted,
            &cand.title,
            &cand.uploader,
            cand.duration,
            cand.official,
            &cand.artists,
            self.tolerance,
        );
        cand.score = scored.score;
        cand.reasons = scored.reasons;
        if scored.rejected.is_some() || scored.score < MIN_SCORE {
            tracing::debug!(
                "reject {} {:?} by {:?}: {:?}",
                cand.source,
                cand.title,
                cand.uploader,
                cand.reasons
            );
            return None;
        }
        Some(cand)
    }

    async fn youtube_music_candidates(&self, wanted: &Wanted) -> Vec<Candidate> {
        let query = format!("{} {}", wanted.artists.join(" "), wanted.title)
            .trim()
            .to_string();
        let mut out: Vec<Candidate> = self
            .ytmusic(&query, Some("songs"), 10)
            .await
            .into_iter()
            .filter_map(|t| {
                self.score(
                    wanted,
                    Candidate {
                        source: "youtube".into(),
                        url: format!("https://music.youtube.com/watch?v={}", t.video_id),
                        reference: t.video_id,
                        title: t.title,
                        uploader: t.artists.join(", "),
                        duration: t.duration,
                        official: true,
                        artists: t.artists,
                        album: t.album,
                        thumbnail: t.thumbnail,
                        ..Default::default()
                    },
                )
            })
            .collect();
        sort_by_score(&mut out);
        out
    }

    async fn flat(&self, source: &str, prefix: &str, wanted: &Wanted) -> Vec<Candidate> {
        let query = format!("{prefix}:{} - {}", wanted.artists.join(", "), wanted.title);
        let mut out = Vec::new();
        for e in self.flat_search(&query).await {
            let (url, reference) = if source == "youtube" {
                let reference = as_str(e.get("id"));
                if reference.len() != 11 {
                    continue;
                }
                (
                    format!("https://www.youtube.com/watch?v={reference}"),
                    reference,
                )
            } else {
                let url = opt_str(e.get("url"))
                    .or_else(|| opt_str(e.get("webpage_url")))
                    .unwrap_or_default();
                if url.is_empty() {
                    continue;
                }
                (url.clone(), opt_str(e.get("id")).unwrap_or(url))
            };
            let uploader = opt_str(e.get("channel"))
                .or_else(|| opt_str(e.get("uploader")))
                .unwrap_or_default();
            let cand = Candidate {
                source: source.into(),
                url,
                reference,
                title: as_str(e.get("title")),
                official: uploader.to_lowercase().ends_with(" - topic"),
                uploader,
                duration: as_f64(e.get("duration")),
                ..Default::default()
            };
            out.extend(self.score(wanted, cand));
        }
        sort_by_score(&mut out);
        out
    }

    /// Acceptable candidates, best first. YouTube Music first; others only if it fails.
    pub async fn find(&self, wanted: &Wanted, exclude: &HashSet<String>) -> Vec<Candidate> {
        let primary: Vec<_> = self
            .youtube_music_candidates(wanted)
            .await
            .into_iter()
            .filter(|c| !exclude.contains(&c.reference))
            .collect();
        if !primary.is_empty() {
            return primary;
        }
        let (youtube, soundcloud) = tokio::join!(
            self.flat("youtube", "ytsearch8", wanted),
            self.flat("soundcloud", "scsearch8", wanted)
        );
        let mut rest: Vec<_> = youtube
            .into_iter()
            .chain(soundcloud)
            .filter(|c| !exclude.contains(&c.reference))
            .collect();
        sort_by_score(&mut rest);
        rest
    }

    // ------------------------------------------------------------- download
    /// Download `cand` into `dest_dir` and return the raw file.
    pub async fn download(
        &self,
        cand: &Candidate,
        dest_dir: &Path,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<PathBuf, SourceError> {
        let format = if cand.source == "youtube" {
            YOUTUBE_FORMAT
        } else {
            SOUNDCLOUD_FORMAT
        };
        let deadline = Instant::now() + timeout;
        if cand.source == "youtube" {
            let info = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(SourceError::Cancelled),
                info = self.backend.resolve(cand.url.clone(), format) => info?,
            };
            match fetch_direct(
                &self.http,
                &info,
                dest_dir,
                deadline,
                4,
                self.fresh_url_grace,
                cancel,
            )
            .await
            {
                Ok(path) => return Ok(path),
                Err(SourceError::Cancelled) => return Err(SourceError::Cancelled),
                Err(e) => tracing::info!(
                    "{}: direct fetch failed ({e}); using yt-dlp",
                    cand.reference
                ),
            }
        }
        self.backend
            .download(
                cand.url.clone(),
                format,
                dest_dir.to_path_buf(),
                cancel.clone(),
                deadline,
            )
            .await
    }
}

fn sort_by_score(list: &mut [Candidate]) {
    list.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Fetch a resolved progressive (non-HLS/DASH-manifest) format with parallel range requests.
pub async fn fetch_direct(
    http: &reqwest::Client,
    info: &Json,
    dest_dir: &Path,
    deadline: Instant,
    parts: u64,
    grace: Duration,
    cancel: &CancellationToken,
) -> Result<PathBuf, SourceError> {
    let url = as_str(info.get("url"));
    let protocol = as_str(info.get("protocol"));
    let size = as_f64(info.get("filesize")).unwrap_or(0.0) as u64;
    if url.is_empty() || !matches!(protocol.as_str(), "http" | "https") || size == 0 {
        return Err(SourceError::new("no direct URL", true));
    }
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(Value::Object(h)) = info.get("http_headers") {
        for (k, v) in h {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::try_from(k.as_str()),
                reqwest::header::HeaderValue::try_from(as_str(Some(v))),
            ) {
                headers.insert(name, value);
            }
        }
    }
    let chunk = (256 * 1024).max(size.div_ceil(parts.max(1)));
    let ranges: Vec<(u64, u64)> = (0..size)
        .step_by(chunk as usize)
        .map(|start| (start, (start + chunk).min(size) - 1))
        .collect();
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_secs(5));
    let started = Instant::now();
    let one = |first: u64, last: u64| {
        let request = http
            .get(&url)
            .headers(headers.clone())
            .header("Range", format!("bytes={first}-{last}"))
            .timeout(remaining);
        async move {
            let response = loop {
                let attempt = request
                    .try_clone()
                    .ok_or_else(|| SourceError::new("direct fetch: request not clonable", true))?;
                let response = attempt
                    .send()
                    .await
                    .map_err(|e| SourceError::new(format!("direct fetch: {e}"), true))?;
                if response.status().as_u16() == 403 && started.elapsed() < grace {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    continue;
                }
                break response;
            };
            if response.status().as_u16() != 206 {
                return Err(SourceError::new(
                    format!("direct fetch: HTTP {}", response.status().as_u16()),
                    true,
                ));
            }
            let mut data = Vec::with_capacity((last - first + 1) as usize);
            let mut stream = response.bytes_stream();
            while let Some(piece) = stream.next().await {
                if cancel.is_cancelled() {
                    return Err(SourceError::Cancelled);
                }
                data.extend_from_slice(
                    &piece.map_err(|e| SourceError::new(format!("direct fetch: {e}"), true))?,
                );
            }
            if data.len() as u64 != last - first + 1 {
                return Err(SourceError::new("direct fetch: short read", true));
            }
            Ok(data)
        }
    };
    let blobs = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(SourceError::Cancelled),
        blobs = futures::future::try_join_all(ranges.iter().map(|(a, b)| one(*a, *b))) => blobs?,
    };
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| SourceError::new(e.to_string(), true))?;
    let ext = opt_str(info.get("ext")).unwrap_or_else(|| "m4a".into());
    let path = dest_dir.join(format!("source.{ext}"));
    tokio::fs::write(&path, blobs.concat())
        .await
        .map_err(|e| SourceError::new(e.to_string(), true))?;
    Ok(path)
}

// ------------------------------------------------------------ YouTube Music
pub const YTMUSIC: &str = "https://music.youtube.com";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0";

fn filter_params(filter: &str) -> Option<&'static str> {
    match filter {
        "songs" => Some("EgWKAQIIAWoMEA4QChADEAQQCRAF"),
        "videos" => Some("EgWKAQIQAWoMEA4QChADEAQQCRAF"),
        "albums" => Some("EgWKAQIYAWoMEA4QChADEAQQCRAF"),
        "artists" => Some("EgWKAQIgAWoMEA4QChADEAQQCRAF"),
        _ => None,
    }
}

fn parse_duration(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if text.trim().is_empty()
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let seconds: u64 = [1u64, 60, 3600]
        .iter()
        .zip(parts.iter().rev())
        .map(|(m, p)| m * p.parse::<u64>().unwrap_or(0))
        .sum();
    Some(seconds as f64)
}

fn runs(column: Option<&Value>) -> Vec<Value> {
    column
        .and_then(|c| {
            path(
                c,
                &["musicResponsiveListItemFlexColumnRenderer", "text", "runs"],
            )
        })
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// One `musicResponsiveListItemRenderer` with a playable video, normalized.
pub fn parse_item(item: &Value, kind: &str) -> Option<YtmTrack> {
    let watch = path(
        item,
        &[
            "overlay",
            "musicItemThumbnailOverlayRenderer",
            "content",
            "musicPlayButtonRenderer",
            "playNavigationEndpoint",
            "watchEndpoint",
        ],
    );
    let video_id = watch
        .and_then(|w| w.get("videoId"))
        .or_else(|| path(item, &["playlistItemData", "videoId"]))
        .and_then(Value::as_str)?
        .to_string();
    let columns = item
        .get("flexColumns")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let title = runs(columns.first())
        .first()
        .map(|r| as_str(r.get("text")))
        .unwrap_or_default();
    let mut line = runs(columns.get(1));
    if let Some(third) = columns.get(2) {
        // A dummy separator keeps the play-count run on an even index.
        line.push(json!({"text": ""}));
        line.extend(runs(Some(third)));
    }
    let (mut artists, mut album, mut duration) = (Vec::new(), None, None);
    for (i, run) in line.iter().enumerate() {
        if i % 2 == 1 {
            continue;
        }
        let text = as_str(run.get("text"));
        match path(run, &["navigationEndpoint", "browseEndpoint", "browseId"])
            .and_then(Value::as_str)
        {
            Some(id) if id.starts_with("MPRE") || id.contains("release_detail") => {
                album = Some(text)
            }
            Some(_) => artists.push(text),
            None => {
                if let Some(d) = parse_duration(&text) {
                    duration = Some(d);
                } else if text.len() == 4 && text.bytes().all(|b| b.is_ascii_digit()) {
                    // a year
                } else if text.starts_with(|c: char| c.is_ascii_digit())
                    && (text.contains(' ') || !text.is_ascii())
                {
                    // a play count
                } else if !text.is_empty() && !matches!(text.as_str(), "Song" | "Video" | "Episode")
                {
                    artists.push(text);
                }
            }
        }
    }
    let thumbnail = path(
        item,
        &[
            "thumbnail",
            "musicThumbnailRenderer",
            "thumbnail",
            "thumbnails",
        ],
    )
    .and_then(Value::as_array)
    .and_then(|t| t.last())
    .and_then(|t| opt_str(t.get("url")));
    let explicit = path(item, &["badges"])
        .and_then(Value::as_array)
        .and_then(|b| b.first())
        .and_then(|b| {
            path(
                b,
                &[
                    "musicInlineBadgeRenderer",
                    "accessibilityData",
                    "accessibilityData",
                    "label",
                ],
            )
        })
        .is_some();
    Some(YtmTrack {
        title: if title.is_empty() {
            video_id.clone()
        } else {
            title
        },
        video_id,
        artists,
        album,
        duration,
        explicit,
        thumbnail,
        kind: kind.into(),
    })
}

/// Rows and the continuation token of a search page (first page or continuation).
pub fn parse_search(response: &Value, kind: &str) -> (Vec<YtmTrack>, Option<String>) {
    let shelves: Vec<Value> = if let Some(cont) = path(
        response,
        &["continuationContents", "musicShelfContinuation"],
    ) {
        vec![cont.clone()]
    } else {
        path(
            response,
            &["contents", "tabbedSearchResultsRenderer", "tabs"],
        )
        .and_then(Value::as_array)
        .and_then(|tabs| tabs.first())
        .and_then(|t| {
            path(
                t,
                &["tabRenderer", "content", "sectionListRenderer", "contents"],
            )
        })
        .and_then(Value::as_array)
        .map(|sections| {
            sections
                .iter()
                .filter_map(|s| s.get("musicShelfRenderer").cloned())
                .collect()
        })
        .unwrap_or_default()
    };
    let mut rows = Vec::new();
    let mut token = None;
    for shelf in shelves {
        for item in shelf
            .get("contents")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(r) = item
                .get("musicResponsiveListItemRenderer")
                .and_then(|i| parse_item(i, kind))
            {
                rows.push(r);
            }
        }
        token = token.or_else(|| {
            shelf
                .get("continuations")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
                .and_then(|c| path(c, &["nextContinuationData", "continuation"]))
                .and_then(Value::as_str)
                .map(String::from)
        });
    }
    (rows, token)
}

fn client_version() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "1.{:04}{:02}{:02}.01.00",
        now.year(),
        now.month() as u8,
        now.day()
    )
}

/// The real backend: YouTube Music innertube over HTTP, everything else through yt-dlp.
pub struct YtBackend {
    http: reqwest::Client,
    ytmusic_url: String,
    visitor: tokio::sync::OnceCell<Option<String>>,
    pub ytdlp: String,
    pub ytdlp_args: Vec<String>,
    /// YouTube player client tried first when resolving (`visionos`: ~2.5 s instead of
    /// ~4.8 s, and its URLs are served at once while the default ones answer 403 for
    /// about a second). yt-dlp's own defaults are the fallback.
    pub fast_client: Option<String>,
}

impl YtBackend {
    pub fn new(ytmusic_url: &str, ytdlp: &str, ytdlp_args: Vec<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(USER_AGENT)
            .build()
            .expect("http client");
        Self {
            http,
            ytmusic_url: ytmusic_url.trim_end_matches('/').into(),
            visitor: Default::default(),
            ytdlp: ytdlp.into(),
            ytdlp_args,
            fast_client: None,
        }
    }

    async fn visitor_id(&self) -> Option<String> {
        self.visitor
            .get_or_init(|| async {
                let html = self
                    .http
                    .get(format!("{}/", self.ytmusic_url))
                    .header("Cookie", "SOCS=CAI")
                    .send()
                    .await
                    .ok()?
                    .text()
                    .await
                    .ok()?;
                let start = html.find("\"VISITOR_DATA\":\"")? + "\"VISITOR_DATA\":\"".len();
                let end = html[start..].find('"')? + start;
                Some(html[start..end].to_string())
            })
            .await
            .clone()
    }

    async fn search_page(
        &self,
        query: &str,
        filter: Option<&str>,
        continuation: Option<&str>,
    ) -> Result<Value, String> {
        let mut body = json!({
            "query": query,
            "context": {"client": {"clientName": "WEB_REMIX", "clientVersion": client_version(), "hl": "en"}, "user": {}},
        });
        if let Some(params) = filter.and_then(filter_params) {
            body["params"] = json!(params);
        }
        let mut url = format!("{}/youtubei/v1/search?alt=json", self.ytmusic_url);
        if let Some(token) = continuation {
            url.push_str(&format!("&ctoken={token}&continuation={token}"));
        }
        let mut request = self
            .http
            .post(url)
            .json(&body)
            .header("Origin", "https://music.youtube.com")
            .header("Cookie", "SOCS=CAI");
        if let Some(visitor) = self.visitor_id().await {
            request = request.header("X-Goog-Visitor-Id", visitor);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response.json().await.map_err(|e| e.to_string())
    }

    async fn search(
        &self,
        query: &str,
        filter: &'static str,
        limit: usize,
    ) -> Result<Vec<YtmTrack>, String> {
        let kind = if filter == "videos" { "video" } else { "song" };
        let (mut rows, mut token) =
            parse_search(&self.search_page(query, Some(filter), None).await?, kind);
        while rows.len() < limit
            && let Some(t) = token.take()
        {
            let (more, next) = parse_search(
                &self.search_page(query, Some(filter), Some(&t)).await?,
                kind,
            );
            if more.is_empty() {
                break;
            }
            rows.extend(more);
            token = next;
        }
        rows.truncate(limit);
        Ok(rows)
    }

    async fn resolve_with(
        &self,
        url: &str,
        format: &str,
        client: &[&str],
    ) -> Result<Json, SourceError> {
        let mut extra: Vec<&str> = client.to_vec();
        extra.extend([
            "-J",
            "--no-playlist",
            "-f",
            format,
            "--no-warnings",
            "--socket-timeout",
            "20",
            "--",
            url,
        ]);
        let out = process::run(
            &self.ytdlp,
            &self.ytdlp_command(&extra),
            Duration::from_secs(90),
            None,
        )
        .await
        .map_err(run_error)?;
        if !out.success() {
            return Err(ytdlp_error(&out.stderr_text()));
        }
        serde_json::from_slice::<Value>(&out.stdout)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .ok_or_else(|| SourceError::new("yt-dlp printed no JSON", true))
    }

    fn ytdlp_command(&self, extra: &[&str]) -> Vec<String> {
        let mut args = self.ytdlp_args.clone();
        args.extend(extra.iter().map(|s| s.to_string()));
        args
    }
}

fn ytdlp_error(text: &str) -> SourceError {
    let permanent = PERMANENT_HINTS
        .iter()
        .any(|h| text.to_lowercase().contains(h));
    SourceError::new(
        format!("yt-dlp: {}", crate::util::tail(text, 500)),
        !permanent,
    )
}

fn run_error(e: RunError) -> SourceError {
    match e {
        RunError::Cancelled => SourceError::Cancelled,
        RunError::Timeout(_) => SourceError::new("yt-dlp timed out", true),
        RunError::Spawn(..) => SourceError::new(e.to_string(), false),
    }
}

impl Backend for YtBackend {
    fn ytmusic_search(
        &self,
        query: String,
        filter: Option<&'static str>,
        limit: usize,
    ) -> BoxFuture<'_, Result<Vec<YtmTrack>, String>> {
        async move {
            match filter {
                Some(f) => self.search(&query, f, limit).await,
                None => {
                    // Unfiltered: official songs first, then videos.
                    let (songs, videos) = tokio::join!(
                        self.search(&query, "songs", limit),
                        self.search(&query, "videos", limit)
                    );
                    if let (Err(e), Err(_)) = (&songs, &videos) {
                        return Err(e.clone());
                    }
                    let mut rows: Vec<YtmTrack> = songs.unwrap_or_default();
                    rows.extend(videos.unwrap_or_default());
                    let mut seen = HashSet::new();
                    rows.retain(|r| seen.insert(r.video_id.clone()));
                    rows.truncate(limit);
                    Ok(rows)
                }
            }
        }
        .boxed()
    }

    fn flat_search(&self, query: String) -> BoxFuture<'_, Result<Vec<Json>, String>> {
        async move {
            let args = self.ytdlp_command(&[
                "--flat-playlist",
                "-J",
                "--no-warnings",
                "--socket-timeout",
                "15",
                "--",
                &query,
            ]);
            let out = process::run(&self.ytdlp, &args, Duration::from_secs(45), None)
                .await
                .map_err(|e| e.to_string())?;
            if !out.success() {
                return Err(crate::util::tail(&out.stderr_text(), 300));
            }
            let info: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
            if info.get("entries").is_none() && info.get("id").is_some() {
                // A single video URL: its own metadata.
                return Ok(info.as_object().cloned().into_iter().collect());
            }
            Ok(info
                .get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_object().cloned())
                .collect())
        }
        .boxed()
    }

    fn resolve(
        &self,
        url: String,
        format: &'static str,
    ) -> BoxFuture<'_, Result<Json, SourceError>> {
        async move {
            let chosen = self.ytdlp_args.iter().any(|a| a.contains("player_client"));
            if let Some(client) = self
                .fast_client
                .as_deref()
                .filter(|c| !c.is_empty() && !chosen)
            {
                match self
                    .resolve_with(
                        &url,
                        format,
                        &[
                            "--extractor-args",
                            &format!("youtube:player_client={client}"),
                        ],
                    )
                    .await
                {
                    Ok(info) => return Ok(info),
                    Err(e) => tracing::info!(
                        "{client} resolve of {url} failed ({e}); using yt-dlp defaults"
                    ),
                }
            }
            self.resolve_with(&url, format, &[]).await
        }
        .boxed()
    }

    fn download(
        &self,
        url: String,
        format: &'static str,
        dest: PathBuf,
        cancel: CancellationToken,
        deadline: Instant,
    ) -> BoxFuture<'_, Result<PathBuf, SourceError>> {
        async move {
            tokio::fs::create_dir_all(&dest)
                .await
                .map_err(|e| SourceError::new(e.to_string(), true))?;
            let template = dest.join("source.%(ext)s").display().to_string();
            let args = self.ytdlp_command(&[
                "-f",
                format,
                "-o",
                &template,
                "--no-playlist",
                "--retries",
                "3",
                "--fragment-retries",
                "3",
                "--socket-timeout",
                "20",
                "--no-part",
                "--force-overwrites",
                "--no-warnings",
                "--quiet",
                "--no-progress",
                "--",
                &url,
            ]);
            let timeout = deadline.saturating_duration_since(Instant::now());
            let out = process::run(&self.ytdlp, &args, timeout, Some(&cancel))
                .await
                .map_err(|e| match e {
                    RunError::Timeout(_) => SourceError::new("download timed out", true),
                    other => run_error(other),
                })?;
            if !out.success() {
                return Err(ytdlp_error(&out.stderr_text()));
            }
            find_source(&dest)
                .ok_or_else(|| SourceError::new("yt-dlp finished without an audio file", true))
        }
        .boxed()
    }
}

fn find_source(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("source."))
        })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testutil::{m, serve};
    use parking_lot::Mutex;

    /// A scripted backend: canned search rows, a resolve result and a downloader.
    #[derive(Default)]
    pub struct FakeBackend {
        pub ytm: Vec<YtmTrack>,
        pub ytm_fails: bool,
        pub flat: Vec<(String, Vec<Json>)>,
        pub flat_calls: Mutex<Vec<String>>,
        pub info: Option<Json>,
        pub download_ext: Option<String>,
        pub download_error: Option<SourceError>,
        pub downloads: Mutex<Vec<(String, String)>>,
    }

    impl Backend for FakeBackend {
        fn ytmusic_search(
            &self,
            _q: String,
            _f: Option<&'static str>,
            _n: usize,
        ) -> BoxFuture<'_, Result<Vec<YtmTrack>, String>> {
            let result = if self.ytm_fails {
                Err("network down".to_string())
            } else {
                Ok(self.ytm.clone())
            };
            async move { result }.boxed()
        }

        fn flat_search(&self, query: String) -> BoxFuture<'_, Result<Vec<Json>, String>> {
            self.flat_calls.lock().push(query.clone());
            let rows = self
                .flat
                .iter()
                .find(|(p, _)| query.starts_with(p.as_str()))
                .map(|(_, r)| r.clone())
                .unwrap_or_default();
            async move { Ok(rows) }.boxed()
        }

        fn resolve(
            &self,
            _url: String,
            _f: &'static str,
        ) -> BoxFuture<'_, Result<Json, SourceError>> {
            let info = self
                .info
                .clone()
                .ok_or_else(|| SourceError::new("no info", true));
            async move { info }.boxed()
        }

        fn download(
            &self,
            url: String,
            format: &'static str,
            dest: PathBuf,
            _c: CancellationToken,
            _d: Instant,
        ) -> BoxFuture<'_, Result<PathBuf, SourceError>> {
            self.downloads.lock().push((url, format.to_string()));
            let err = self.download_error.clone();
            let ext = self.download_ext.clone().unwrap_or_else(|| "m4a".into());
            async move {
                if let Some(e) = err {
                    return Err(e);
                }
                std::fs::create_dir_all(&dest).unwrap();
                let p = dest.join(format!("source.{ext}"));
                std::fs::write(&p, b"x").unwrap();
                Ok(p)
            }
            .boxed()
        }
    }

    fn ytm(
        id: &str,
        title: &str,
        artists: &[&str],
        album: Option<&str>,
        duration: f64,
    ) -> YtmTrack {
        YtmTrack {
            video_id: id.into(),
            title: title.into(),
            artists: artists.iter().map(|a| a.to_string()).collect(),
            album: album.map(String::from),
            duration: Some(duration),
            kind: "song".into(),
            ..Default::default()
        }
    }

    fn ytm_rows() -> Vec<YtmTrack> {
        vec![
            ytm(
                "YUyG_nM3Rig",
                "порезы на руках",
                &["FORTUNA 812", "elox1m"],
                Some("x"),
                144.0,
            ),
            ytm(
                "Q1QvyIrUzz0",
                "Порезы на руках",
                &["Юпи"],
                Some("Порезы на руках"),
                124.0,
            ),
            ytm(
                "gvniJHIecH4",
                "Порезы на руках (sped up)",
                &["Юпи"],
                None,
                124.0,
            ),
        ]
    }

    fn wanted() -> Wanted {
        Wanted {
            duration: Some(124.0),
            ..Wanted::new("Порезы на руках", &["Юпи"])
        }
    }

    fn finder(backend: FakeBackend) -> (Sources, Arc<FakeBackend>) {
        let backend = Arc::new(backend);
        (Sources::new(10.0, backend.clone()), backend)
    }

    #[tokio::test]
    async fn youtube_music_is_preferred_and_filtered() {
        let (f, backend) = finder(FakeBackend {
            ytm: ytm_rows(),
            ..Default::default()
        });
        let found = f.find(&wanted(), &HashSet::new()).await;
        assert_eq!(
            found
                .iter()
                .map(|c| c.reference.as_str())
                .collect::<Vec<_>>(),
            ["Q1QvyIrUzz0"]
        );
        assert!(found[0].official && found[0].album.as_deref() == Some("Порезы на руках"));
        assert!(
            backend.flat_calls.lock().is_empty(),
            "fallbacks are not searched"
        );
    }

    #[tokio::test]
    async fn fallback_searches_youtube_and_soundcloud() {
        let backend = FakeBackend {
            flat: vec![
                (
                    "ytsearch".into(),
                    vec![m(
                        json!({"id": "AAAAAAAAAAA", "title": "Юпи - Порезы на руках (минус)", "channel": "x", "duration": 124}),
                    )],
                ),
                (
                    "scsearch".into(),
                    vec![m(
                        json!({"id": "2398309779", "url": "https://api.soundcloud.com/tracks/2398309779", "title": "Юпи - Порезы на руках", "uploader": "ima", "duration": 121.8}),
                    )],
                ),
            ],
            ..Default::default()
        };
        let (f, _) = finder(backend);
        let found = f.find(&wanted(), &HashSet::new()).await;
        assert_eq!(
            found
                .iter()
                .map(|c| (c.source.as_str(), c.reference.as_str()))
                .collect::<Vec<_>>(),
            [("soundcloud", "2398309779")]
        );
    }

    #[tokio::test]
    async fn search_failure_is_not_fatal_and_exclusions_apply() {
        let (f, _) = finder(FakeBackend {
            ytm_fails: true,
            ..Default::default()
        });
        assert!(f.find(&wanted(), &HashSet::new()).await.is_empty());
        let (f, _) = finder(FakeBackend {
            ytm: ytm_rows(),
            ..Default::default()
        });
        assert!(
            f.find(&wanted(), &HashSet::from(["Q1QvyIrUzz0".to_string()]))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn soundcloud_download_uses_its_format_and_propagates_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cand = Candidate {
            source: "soundcloud".into(),
            url: "https://sc/1".into(),
            reference: "1".into(),
            ..Default::default()
        };
        let (f, backend) = finder(FakeBackend::default());
        let token = CancellationToken::new();
        let path = f
            .download(
                &cand,
                &dir.path().join("job"),
                Duration::from_secs(5),
                &token,
            )
            .await
            .unwrap();
        assert_eq!(path, dir.path().join("job/source.m4a"));
        assert_eq!(backend.downloads.lock()[0].1, SOUNDCLOUD_FORMAT);
        let (f, _) = finder(FakeBackend {
            download_error: Some(SourceError::new("DRM protected", false)),
            ..Default::default()
        });
        let err = f
            .download(
                &cand,
                &dir.path().join("job2"),
                Duration::from_secs(5),
                &token,
            )
            .await
            .unwrap_err();
        assert!(!err.transient());
    }

    fn body() -> Vec<u8> {
        (0..4000).flat_map(|_| 0u8..=255).collect()
    }

    async fn range_server(status: u16) -> (crate::testutil::Server, Arc<Mutex<Vec<String>>>) {
        range_server_refusing(status, 0).await
    }

    /// Answers `status` after refusing the first `refusals` requests with 403.
    async fn range_server_refusing(
        status: u16,
        refusals: usize,
    ) -> (crate::testutil::Server, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let router = axum::Router::new().route(
            "/audio",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let s = s.clone();
                async move {
                    let range = headers["range"]
                        .to_str()
                        .unwrap()
                        .trim_start_matches("bytes=")
                        .to_string();
                    let n = {
                        let mut seen = s.lock();
                        seen.push(range.clone());
                        seen.len()
                    };
                    let (a, b) = range.split_once('-').unwrap();
                    let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                    let status = if n <= refusals { 403 } else { status };
                    if status != 206 {
                        return axum::response::Response::builder()
                            .status(status)
                            .body(axum::body::Body::empty())
                            .unwrap();
                    }
                    axum::response::Response::builder()
                        .status(206)
                        .body(axum::body::Body::from(body()[a..=b].to_vec()))
                        .unwrap()
                }
            }),
        );
        (serve(router).await, seen)
    }

    fn info(url: &str) -> Json {
        m(
            json!({"url": format!("{url}/audio"), "protocol": "https", "filesize": body().len(), "ext": "m4a"}),
        )
    }

    #[tokio::test]
    async fn direct_fetch_joins_parallel_ranges() {
        let (server, seen) = range_server(206).await;
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let path = fetch_direct(
            &reqwest::Client::new(),
            &info(&server.url),
            dir.path(),
            Instant::now() + Duration::from_secs(30),
            4,
            Duration::ZERO,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(path.file_name().unwrap(), "source.m4a");
        assert_eq!(std::fs::read(&path).unwrap(), body());
        assert_eq!(seen.lock().len(), 4);
    }

    #[tokio::test]
    async fn direct_fetch_waits_out_the_403_of_a_fresh_url() {
        let (server, seen) = range_server_refusing(206, 2).await;
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        let path = fetch_direct(
            &reqwest::Client::new(),
            &info(&server.url),
            dir.path(),
            deadline,
            4,
            Duration::from_secs(5),
            &token,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), body());
        assert_eq!(seen.lock().len(), 6, "two refused requests were retried");
    }

    #[tokio::test]
    async fn download_falls_back_to_ytdlp_when_the_direct_fetch_is_refused() {
        let (server, _) = range_server(403).await;
        let dir = tempfile::tempdir().unwrap();
        let (mut f, backend) = finder(FakeBackend {
            info: Some(info(&server.url)),
            download_ext: Some("webm".into()),
            ..Default::default()
        });
        f.fresh_url_grace = Duration::from_millis(300);
        let cand = Candidate {
            source: "youtube".into(),
            url: "https://youtube.com/watch?v=x".into(),
            ..Default::default()
        };
        let path = f
            .download(
                &cand,
                &dir.path().join("job"),
                Duration::from_secs(30),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(path.file_name().unwrap(), "source.webm");
        assert_eq!(backend.downloads.lock()[0].1, YOUTUBE_FORMAT);
    }

    #[tokio::test]
    async fn direct_fetch_needs_a_progressive_url_and_stops_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let hls = m(json!({"url": "https://x/m.m3u8", "protocol": "m3u8_native"}));
        assert!(
            fetch_direct(
                &reqwest::Client::new(),
                &hls,
                dir.path(),
                Instant::now(),
                4,
                Duration::ZERO,
                &token
            )
            .await
            .is_err()
        );

        let (server, _) = range_server(206).await;
        let (f, backend) = finder(FakeBackend {
            info: Some(info(&server.url)),
            ..Default::default()
        });
        token.cancel();
        let cand = Candidate {
            source: "youtube".into(),
            url: "u".into(),
            ..Default::default()
        };
        let err = f
            .download(
                &cand,
                &dir.path().join("job"),
                Duration::from_secs(30),
                &token,
            )
            .await
            .unwrap_err();
        assert_eq!(err, SourceError::Cancelled);
        assert!(backend.downloads.lock().is_empty(), "yt-dlp must not run");
    }

    #[test]
    fn parses_a_real_youtube_music_songs_page() {
        let raw: Value =
            serde_json::from_str(include_str!("../tests/fixtures/ytmusic_songs.json")).unwrap();
        let (rows, token) = parse_search(&raw, "song");
        assert!(token.is_some());
        assert!(rows.len() >= 10);
        let first = &rows[0];
        assert_eq!(first.video_id, "4D7u5KF7SP8");
        assert_eq!(
            first.title,
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)"
        );
        assert_eq!(
            first.artists,
            ["Daft Punk", "Pharrell Williams", "Nile Rodgers"]
        );
        assert_eq!(first.album.as_deref(), Some("Random Access Memories"));
        assert_eq!(first.duration, Some(370.0));
        assert!(!first.explicit);
        assert!(first.thumbnail.as_deref().unwrap().starts_with("https://"));
        assert_eq!(rows[1].video_id, "Rgrt_8mXrK8");
        assert_eq!(rows[1].duration, Some(249.0));
    }

    #[test]
    fn durations_and_errors() {
        assert_eq!(parse_duration("6:10"), Some(370.0));
        assert_eq!(parse_duration("1:02:03"), Some(3723.0));
        assert_eq!(parse_duration("2,343"), None);
        assert!(!ytdlp_error("ERROR: Video unavailable").transient());
        assert!(ytdlp_error("HTTP Error 503").transient());
        assert!(client_version().starts_with("1.20"));
    }

    #[tokio::test]
    async fn innertube_search_posts_the_songs_filter() {
        let raw: Value =
            serde_json::from_str(include_str!("../tests/fixtures/ytmusic_songs.json")).unwrap();
        let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
        let s = seen.clone();
        let router = axum::Router::new()
            .route(
                "/",
                axum::routing::get(|| async {
                    r#"<script>ytcfg.set({"VISITOR_DATA":"Cgt4eXo%3D"});</script>"#
                }),
            )
            .route(
                "/youtubei/v1/search",
                axum::routing::post(
                    move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                        let (s, raw) = (s.clone(), raw.clone());
                        async move {
                            assert_eq!(headers["x-goog-visitor-id"], "Cgt4eXo%3D");
                            s.lock().push(body);
                            axum::Json(raw)
                        }
                    },
                ),
            );
        let server = serve(router).await;
        let backend = YtBackend::new(&server.url, "yt-dlp", vec![]);
        let rows = backend
            .ytmusic_search("Daft Punk Get Lucky".into(), Some("songs"), 5)
            .await
            .unwrap();
        assert_eq!(rows.len(), 5);
        let body = &seen.lock()[0];
        assert_eq!(body["params"], "EgWKAQIIAWoMEA4QChADEAQQCRAF");
        assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
    }

    #[tokio::test]
    async fn resolve_tries_the_fast_client_first_then_yt_dlp_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let (log, script) = (dir.path().join("calls"), dir.path().join("yt-dlp"));
        let body = format!(
            "#!/bin/sh\necho \"$*\" >> {log}\ncase \"$*\" in *player_client=visionos*) [ -f {ok} ] && {{ echo '{{\"url\":\"fast\"}}'; exit 0; }}; echo 'ERROR: no' >&2; exit 1;; esac\necho '{{\"url\":\"default\"}}'\n",
            log = log.display(),
            ok = dir.path().join("fast-ok").display()
        );
        std::fs::write(&script, body).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut backend = YtBackend::new("http://unused", script.to_str().unwrap(), vec![]);
        backend.fast_client = Some("visionos".into());
        let calls = || {
            std::fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .count()
        };
        assert_eq!(
            backend.resolve("u".into(), YOUTUBE_FORMAT).await.unwrap()["url"],
            "default"
        );
        assert_eq!(
            calls(),
            2,
            "fast client failed: one more call with defaults"
        );
        std::fs::write(dir.path().join("fast-ok"), "").unwrap();
        assert_eq!(
            backend.resolve("u".into(), YOUTUBE_FORMAT).await.unwrap()["url"],
            "fast"
        );
        assert_eq!(calls(), 3);
        backend.ytdlp_args = vec![
            "--extractor-args".into(),
            "youtube:player_client=ios".into(),
        ];
        assert_eq!(
            backend.resolve("u".into(), YOUTUBE_FORMAT).await.unwrap()["url"],
            "default",
            "an operator's choice wins"
        );
    }
}

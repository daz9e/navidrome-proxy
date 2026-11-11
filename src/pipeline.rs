//! From an external track id to a playable, tagged file in the library.

use crate::audio::Tools;
use crate::config::Settings;
use crate::covers::Covers;
use crate::db::{Field, Store, Track};
use crate::deezer::Deezer;
use crate::identity::Identity;
use crate::ids::{self, Kind};
use crate::library::{Indexer, Library};
use crate::matching::Wanted;
use crate::sources::{Candidate, SourceError, Sources};
use crate::tasks::Background;
use crate::util::{Json, as_f64, as_str, opt_str, truthy};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Where audio comes from: searching candidates and downloading one.
pub trait Fetcher: Send + Sync {
    fn find<'a>(&'a self, wanted: &'a Wanted, exclude: &'a HashSet<String>) -> BoxFuture<'a, Vec<Candidate>>;
    fn download<'a>(&'a self, cand: &'a Candidate, dest: &'a Path, timeout: Duration, cancel: &'a CancellationToken) -> BoxFuture<'a, Result<PathBuf, SourceError>>;
}

impl Fetcher for Sources {
    fn find<'a>(&'a self, wanted: &'a Wanted, exclude: &'a HashSet<String>) -> BoxFuture<'a, Vec<Candidate>> {
        Box::pin(Sources::find(self, wanted, exclude))
    }

    fn download<'a>(&'a self, cand: &'a Candidate, dest: &'a Path, timeout: Duration, cancel: &'a CancellationToken) -> BoxFuture<'a, Result<PathBuf, SourceError>> {
        Box::pin(Sources::download(self, cand, dest, timeout, cancel))
    }
}

pub struct Pipeline {
    pub settings: Arc<Settings>,
    pub store: Arc<Store>,
    pub identity: Identity,
    pub deezer: Arc<Deezer>,
    pub fetcher: Arc<dyn Fetcher>,
    pub library: Arc<Library>,
    pub indexer: Option<Arc<Indexer>>,
    pub covers: Arc<Covers>,
    pub tools: Tools,
    pub lyrics_url: String,
    pub background: Arc<Background>,
}

/// Cover bytes ready to embed: MP4 and most ID3 readers know only JPEG/PNG artwork.
pub async fn cover_for_tags(covers: &Covers, tools: &Tools, url: &str, work_dir: &Path) -> Option<Vec<u8>> {
    let cover = covers.get(url).await?.0;
    if crate::covers::is_webp(&cover) { tools.to_jpeg(&cover, work_dir).await } else { Some(cover) }
}

fn io_error(e: impl std::fmt::Display) -> SourceError {
    SourceError::new(e.to_string(), true)
}

impl Pipeline {
    // ---------------------------------------------------------------- helpers
    /// Complete catalog metadata for tagging and matching.
    pub async fn hydrate(&self, track: &Track) -> Result<Json, SourceError> {
        let mut meta = track.meta.clone();
        if let Some(vid) = ids::parse(&track.id)
            && vid.kind == Kind::Track
            && !truthy(meta.get("full"))
        {
            match self.deezer.full_track_meta(&vid.key).await {
                Ok(full) => {
                    self.store.remember_track(&track.id, &full);
                    meta.extend(full.into_iter().filter(|(_, v)| !v.is_null()));
                }
                Err(e) if opt_str(meta.get("title")).is_none() || opt_str(meta.get("artist")).is_none() => {
                    return Err(SourceError::new(format!("Deezer metadata unavailable: {e}"), true));
                }
                Err(e) => tracing::info!("using search metadata for {}: {e}", track.id),
            }
        }
        if opt_str(meta.get("title")).is_none() {
            return Err(SourceError::new(format!("no metadata for {}", track.id), false));
        }
        Ok(meta)
    }

    fn direct_candidate(track_id: &str, meta: &Json) -> Option<Candidate> {
        let vid = ids::parse(track_id)?;
        let base = |source: &str, url: String| Candidate {
            source: source.into(),
            url,
            reference: vid.key.clone(),
            title: as_str(meta.get("title")),
            uploader: as_str(meta.get("artist")),
            duration: as_f64(meta.get("duration")),
            ..Default::default()
        };
        match vid.kind {
            Kind::Youtube => Some(base("youtube", format!("https://www.youtube.com/watch?v={}", vid.key))),
            Kind::Soundcloud => opt_str(meta.get("url")).map(|url| base("soundcloud", url)),
            _ => None,
        }
    }

    /// Mark `track_id` as an alias of the already local `existing` and return it.
    ///
    /// The requested row gets the terminal status `alias`, so it never stays
    /// "downloading"; it keeps no file of its own (the canonical track owns it).
    fn alias_of(&self, track_id: &str, existing: Track, how: &str) -> Track {
        self.identity.link(track_id, &existing.id, how);
        self.store.update_track(track_id, &[Field::status("alias"), Field::Error(None)]);
        existing
    }

    // --------------------------------------------------------------- download
    /// Make `track_id` playable. Returns the track that now backs it.
    ///
    /// That may be another, already downloaded recording of the same song, in
    /// which case `track_id` becomes an alias of it.
    pub async fn download(&self, track_id: &str, cancel: &CancellationToken) -> Result<Track, SourceError> {
        let track = self.store.get_track(track_id).ok_or_else(|| SourceError::new(format!("unknown track {track_id}"), false))?;
        if track.is_ready() {
            return Ok(track);
        }
        let meta = self.hydrate(&track).await?;
        if let Some(existing) = self.identity.find_existing(&meta, Some(track_id), true) {
            tracing::info!("{track_id} is the same recording as {}; not downloading again", existing.id);
            return Ok(self.alias_of(track_id, existing, "recording"));
        }
        let direct = Self::direct_candidate(track_id, &meta);
        let artists: Vec<String> = match meta.get("artists").and_then(Value::as_array) {
            Some(a) if !a.is_empty() => a.iter().map(|v| as_str(Some(v))).collect(),
            _ => vec![as_str(meta.get("artist"))],
        };
        let wanted = Wanted {
            title: as_str(meta.get("title")),
            artists,
            duration: as_f64(meta.get("duration")).filter(|d| *d != 0.0),
            ..Default::default()
        };
        let mut tried = HashSet::new();
        let mut candidates = match &direct {
            Some(c) => vec![c.clone()],
            None => self.fetcher.find(&wanted, &tried).await,
        };
        if candidates.is_empty() {
            return Err(SourceError::new("no matching audio on YouTube Music, YouTube or SoundCloud", false));
        }
        // A found source may itself already be in the library under its own id.
        for cand in &candidates {
            let source_id = if cand.source == "youtube" { ids::youtube(&cand.reference) } else { ids::soundcloud(&cand.reference) };
            if let Some(known) = self.store.get_track(&self.identity.canonical(&source_id))
                && known.id != track_id
                && known.is_ready()
            {
                return Ok(self.alias_of(track_id, known, "source"));
            }
        }
        let mut errors: Vec<SourceError> = Vec::new();
        for round in 0..2 {
            for cand in candidates.iter().take(3) {
                tried.insert(cand.reference.clone());
                match self.fetch(&track, &meta, cand, cancel).await {
                    Ok(t) => return Ok(t),
                    Err(SourceError::Cancelled) => return Err(SourceError::Cancelled),
                    Err(e) => {
                        tracing::warn!("{track_id}: {} candidate {} failed: {e}", cand.source, cand.reference);
                        errors.push(e);
                    }
                }
            }
            // Age-gated or DRM results on YouTube Music: look again on YouTube and SoundCloud.
            if round > 0 || direct.is_some() || errors.iter().any(SourceError::transient) {
                break;
            }
            candidates = self.fetcher.find(&wanted, &tried).await;
        }
        let transient = errors.iter().any(SourceError::transient);
        let message: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
        Err(SourceError::new(crate::util::tail(&message.join("; "), 800), transient))
    }

    async fn fetch(&self, track: &Track, meta: &Json, cand: &Candidate, cancel: &CancellationToken) -> Result<Track, SourceError> {
        let staging = self.library.new_staging(&track.id).map_err(io_error)?;
        let result = self.fetch_into(track, meta, cand, cancel, &staging).await;
        let _ = tokio::fs::remove_dir_all(&staging).await;
        result
    }

    async fn fetch_into(&self, track: &Track, meta: &Json, cand: &Candidate, cancel: &CancellationToken, staging: &Path) -> Result<Track, SourceError> {
        let raw = self.fetcher.download(cand, staging, Duration::from_secs(300), cancel).await?;
        let (prepared, info) = self.tools.normalize(&raw, staging, "prepared").await.map_err(|e| SourceError::new(e.0, false))?;
        let expected = as_f64(meta.get("duration")).unwrap_or(0.0);
        if expected != 0.0 && (info.duration - expected).abs() > self.settings.duration_tolerance_seconds {
            return Err(SourceError::new(format!("duration {:.0}s does not match {expected:.0}s", info.duration), false));
        }
        let mut tag_meta = meta.clone();
        if !truthy(tag_meta.get("album"))
            && let Some(album) = cand.album.clone().filter(|a| !a.is_empty())
            && (track.id.starts_with("yt:") || track.id.starts_with("sc:"))
        {
            tag_meta.insert("album".into(), json!(album));
        }
        let cover_url = opt_str(tag_meta.get("cover")).or_else(|| cand.thumbnail.clone()).unwrap_or_default();
        let cover = cover_for_tags(&self.covers, &self.tools, &cover_url, staging).await;
        let (path, tags) = (prepared.clone(), tag_meta.clone());
        tokio::task::spawn_blocking(move || crate::tagging::write_tags(&path, &tags, cover.as_deref()))
            .await
            .map_err(io_error)?
            .map_err(|e| SourceError::new(format!("tagging failed: {e}"), false))?;
        let (library, id, prep, tm, previous) = (self.library.clone(), track.id.clone(), prepared.clone(), tag_meta.clone(), track.file.clone());
        let dest = tokio::task::spawn_blocking(move || library.publish(&id, &prep, &tm, previous.as_deref())).await.map_err(io_error)?.map_err(io_error)?;
        let mut audio = info.as_json();
        audio.insert("size".into(), json!(std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0)));
        self.store.update_track(
            &track.id,
            &[
                Field::status("ready"),
                Field::File(Some(dest.display().to_string())),
                Field::Source(Some(cand.source.clone())),
                Field::SourceRef(Some(cand.reference.clone())),
                Field::Audio(Some(audio)),
                Field::Error(None),
            ],
        );
        if tag_meta.get("album") != meta.get("album") {
            let mut patch = Json::new();
            patch.insert("album".into(), tag_meta["album"].clone());
            self.store.remember_track(&track.id, &patch);
        }
        let fresh = self.store.get_track(&track.id).ok_or_else(|| SourceError::new("track vanished", true))?;
        self.identity.register(&fresh);
        if let Some(indexer) = &self.indexer {
            indexer.notify();
        }
        if self.settings.lyrics {
            let (http, url, dest, meta) = (self.covers.http.clone(), self.lyrics_url.clone(), dest.clone(), tag_meta.clone());
            self.background.spawn(async move {
                if let Some((kind, text)) = crate::lyrics::fetch(&http, &meta, &url).await
                    && dest.exists()
                    && let Err(e) = crate::lyrics::save(&dest, kind, &text)
                {
                    tracing::warn!("saving lyrics for {} failed: {e}", dest.display());
                }
            });
        }
        if let Some(album_id) = fresh.album_id.clone().filter(|_| track.id.starts_with("deezer:")) {
            // Warm the album's track list: local album pages are completed from it.
            let deezer = self.deezer.clone();
            self.background.spawn(async move {
                let _ = deezer.album_tracks(&album_id).await;
            });
        }
        tracing::info!(
            "ready {} <- {} {} ({:.0}s, {})",
            track.id,
            cand.source,
            cand.reference,
            info.duration,
            dest.file_name().and_then(|n| n.to_str()).unwrap_or("")
        );
        Ok(fresh)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::audio::tests::{have, make, need};
    use crate::testutil::{FakeDeezer, dz_track, m, serve};
    use parking_lot::Mutex;
    use std::collections::HashMap;

    /// Scripted candidates per search and synthetic audio per candidate ref.
    #[derive(Default)]
    pub struct FakeFetcher {
        pub candidates: Mutex<Vec<Candidate>>,
        pub results: Mutex<HashMap<String, Vec<Candidate>>>,
        /// ref -> seconds of audio; `None` = transient 403; `Some(0)` = age gate.
        pub lengths: Mutex<HashMap<String, Option<u32>>>,
        pub finds: Mutex<usize>,
        pub fetched: Mutex<Vec<String>>,
    }

    impl Fetcher for FakeFetcher {
        fn find<'a>(&'a self, wanted: &'a Wanted, exclude: &'a HashSet<String>) -> BoxFuture<'a, Vec<Candidate>> {
            *self.finds.lock() += 1;
            let list = self.results.lock().get(&wanted.title).cloned().unwrap_or_else(|| self.candidates.lock().clone());
            let out = list.into_iter().filter(|c| !exclude.contains(&c.reference)).collect();
            Box::pin(async move { out })
        }

        fn download<'a>(&'a self, cand: &'a Candidate, dest: &'a Path, _t: Duration, _c: &'a CancellationToken) -> BoxFuture<'a, Result<PathBuf, SourceError>> {
            self.fetched.lock().push(cand.reference.clone());
            let length = self.lengths.lock().get(&cand.reference).cloned().unwrap_or(Some(30));
            let dest = dest.to_path_buf();
            Box::pin(async move {
                match length {
                    None => Err(SourceError::new("HTTP Error 403", true)),
                    Some(0) => Err(SourceError::new("Sign in to confirm your age", false)),
                    Some(n) => Ok(tokio::task::spawn_blocking(move || make(&dest.join("source.m4a"), n, "aac", true)).await.unwrap()),
                }
            })
        }
    }

    pub fn cand(reference: &str, source: &str) -> Candidate {
        Candidate {
            source: source.into(),
            url: format!("https://x/{reference}"),
            reference: reference.into(),
            title: "Hella Quality".into(),
            uploader: "TVETH - Topic".into(),
            duration: Some(30.0),
            ..Default::default()
        }
    }

    struct World {
        store: Arc<Store>,
        settings: Arc<Settings>,
        _server: crate::testutil::Server,
        _dir: tempfile::TempDir,
        deezer: Arc<Deezer>,
        covers: Arc<Covers>,
    }

    impl World {
        async fn new() -> Self {
            let fake = FakeDeezer::empty();
            let mut t = dz_track(1, "Hella Quality", "TVETH", 100, "RAW", 3, &["TVETH", "Bato"]);
            t.insert("duration".into(), json!(30));
            fake.add_album(100, "RAW", "TVETH", 1, vec![t]);
            let server = serve(fake.router()).await;
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(Store::memory());
            let deezer = Arc::new(Deezer::new(store.clone(), 100.0, &server.url, 60.0));
            let env: HashMap<String, String> = [
                ("MUSIC_DIR", dir.path().join("music").display().to_string()),
                ("DATA_DIR", dir.path().display().to_string()),
                ("LYRICS", "false".into()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
            let settings = Arc::new(Settings::from_env(&env));
            let covers = Arc::new(Covers::new(&settings.cache_dir().join("covers")));
            Self { store, settings, _server: server, _dir: dir, deezer, covers }
        }

        fn pipeline(&self, fetcher: Arc<FakeFetcher>) -> Pipeline {
            Pipeline {
                settings: self.settings.clone(),
                store: self.store.clone(),
                identity: Identity::new(self.store.clone()),
                deezer: self.deezer.clone(),
                fetcher,
                library: Arc::new(Library::new(&self.settings.music_dir, self.settings.staging_dir())),
                indexer: None,
                covers: self.covers.clone(),
                tools: Tools::default(),
                lyrics_url: String::new(),
                background: Arc::new(Background::default()),
            }
        }

        fn remember(&self, id: &str, meta: Value) {
            self.store.remember_track(id, &m(meta));
        }
    }

    fn fetcher(candidates: Vec<Candidate>, lengths: &[(&str, Option<u32>)]) -> Arc<FakeFetcher> {
        let f = FakeFetcher::default();
        *f.candidates.lock() = candidates;
        f.lengths.lock().extend(lengths.iter().map(|(k, v)| (k.to_string(), *v)));
        Arc::new(f)
    }

    fn hella() -> Value {
        json!({"title": "Hella Quality", "artist": "TVETH", "duration": 30})
    }

    #[tokio::test]
    async fn download_tags_and_publishes_in_library_layout() {
        need!("ffmpeg");
        let w = World::new().await;
        w.remember("deezer:1", hella());
        let p = w.pipeline(fetcher(vec![cand("dEvrb6N70kA", "youtube")], &[]));
        let track = p.download("deezer:1", &CancellationToken::new()).await.unwrap();
        assert_eq!((track.status.as_str(), track.source.as_deref()), ("ready", Some("youtube")));
        let path = w.settings.music_dir.join("TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a");
        assert_eq!(track.file.as_deref(), Some(path.to_str().unwrap()));
        let tags = crate::tagging::read_tags(&path);
        assert_eq!((tags["artist"][0].as_str(), tags["album"][0].as_str()), ("TVETH, Bato", "RAW"));
        assert_eq!(track.audio["codec"], "aac");
        assert_ne!(track.audio["brand"], "dash");
        assert!((track.audio["duration"].as_f64().unwrap() - 30.0).abs() < 0.5);
        assert_eq!(track.audio["size"], json!(std::fs::metadata(&path).unwrap().len()));
        // The YouTube source is now an alias of the Deezer track.
        assert_eq!(p.identity.canonical("yt:dEvrb6N70kA"), "deezer:1");
        assert_eq!(std::fs::read_dir(w.settings.staging_dir()).unwrap().count(), 0);
        assert!(p.download("deezer:1", &CancellationToken::new()).await.unwrap().is_ready(), "ready is a no-op");
    }

    #[tokio::test]
    async fn wrong_duration_candidate_is_skipped_for_the_next() {
        need!("ffmpeg");
        let w = World::new().await;
        w.remember("deezer:1", hella());
        let f = fetcher(vec![cand("AAAAAAAAAAA", "youtube"), cand("BBBBBBBBBBB", "youtube")], &[("AAAAAAAAAAA", Some(60)), ("BBBBBBBBBBB", Some(31))]);
        let track = w.pipeline(f.clone()).download("deezer:1", &CancellationToken::new()).await.unwrap();
        assert_eq!(*f.fetched.lock(), ["AAAAAAAAAAA", "BBBBBBBBBBB"]);
        assert_eq!(track.source_ref.as_deref(), Some("BBBBBBBBBBB"));
    }

    #[tokio::test]
    async fn all_candidates_failing_transiently_is_retryable() {
        let w = World::new().await;
        w.remember("deezer:1", hella());
        let f = fetcher(vec![cand("AAAAAAAAAAA", "youtube")], &[("AAAAAAAAAAA", None)]);
        let err = w.pipeline(f.clone()).download("deezer:1", &CancellationToken::new()).await.unwrap_err();
        assert!(err.transient() && err.to_string().contains("403"));
        assert_eq!(*f.finds.lock(), 1, "no second search after a transient failure");
    }

    #[tokio::test]
    async fn permanent_failures_trigger_a_second_search() {
        need!("ffmpeg");
        let w = World::new().await;
        w.remember("deezer:1", hella());
        let f = fetcher(
            vec![cand("AAAAAAAAAAA", "youtube"), cand("BBBBBBBBBBB", "youtube"), cand("CCCCCCCCCCC", "youtube"), cand("DDDDDDDDDDD", "soundcloud")],
            &[("AAAAAAAAAAA", Some(0)), ("BBBBBBBBBBB", Some(0)), ("CCCCCCCCCCC", Some(0)), ("DDDDDDDDDDD", Some(30))],
        );
        let track = w.pipeline(f.clone()).download("deezer:1", &CancellationToken::new()).await.unwrap();
        assert_eq!(*f.finds.lock(), 2);
        assert_eq!(f.fetched.lock().last().unwrap(), "DDDDDDDDDDD");
        assert_eq!(track.source_ref.as_deref(), Some("DDDDDDDDDDD"));
    }

    #[tokio::test]
    async fn no_candidates_unknown_tracks_and_missing_metadata_are_permanent() {
        let w = World::new().await;
        w.remember("deezer:1", hella());
        let p = w.pipeline(fetcher(vec![], &[]));
        let c = CancellationToken::new();
        assert!(!p.download("deezer:1", &c).await.unwrap_err().transient());
        assert!(!p.download("deezer:404", &c).await.unwrap_err().transient(), "unknown track");
        w.remember("yt:sO4pSj466Hk", json!({"artist": "x"}));
        assert!(!p.download("yt:sO4pSj466Hk", &c).await.unwrap_err().transient(), "no title");
        // Deezer down and no search metadata to fall back on: retry later.
        w.remember("deezer:999", json!({"title": "only title"}));
        assert!(p.download("deezer:999", &c).await.unwrap_err().transient());
    }

    #[tokio::test]
    async fn same_recording_under_another_id_is_not_downloaded_again() {
        need!("ffmpeg");
        let w = World::new().await;
        w.remember("sc:77", json!({"title": "Hella Quality", "artists": ["TVETH"], "duration": 31, "url": "https://soundcloud/77"}));
        let f = fetcher(vec![], &[("77", Some(31))]);
        let p = w.pipeline(f.clone());
        let c = CancellationToken::new();
        assert_eq!(p.download("sc:77", &c).await.unwrap().id, "sc:77");
        assert_eq!(*f.fetched.lock(), ["77"]);
        w.remember("deezer:1", hella());
        w.store.update_track("deezer:1", &[Field::status("downloading")]);
        assert_eq!(p.download("deezer:1", &c).await.unwrap().id, "sc:77");
        assert_eq!((f.fetched.lock().len(), *f.finds.lock()), (1, 0));
        assert_eq!(p.identity.canonical("deezer:1"), "sc:77");
        // B2: the requested track does not stay "downloading" forever.
        assert_eq!(w.store.get_track("deezer:1").unwrap().status, "alias");
    }

    #[tokio::test]
    async fn found_source_already_in_library_becomes_the_backing_track() {
        let w = World::new().await;
        let file = w.settings.music_dir.join("x.m4a");
        std::fs::create_dir_all(&w.settings.music_dir).unwrap();
        std::fs::write(&file, b"x").unwrap();
        w.remember("yt:dEvrb6N70kA", json!({"title": "Other Name", "artist": "Someone", "duration": 30}));
        w.store.update_track("yt:dEvrb6N70kA", &[Field::status("ready"), Field::File(Some(file.display().to_string()))]);
        w.remember("deezer:1", hella());
        let f = fetcher(vec![cand("dEvrb6N70kA", "youtube")], &[]);
        let backing = w.pipeline(f.clone()).download("deezer:1", &CancellationToken::new()).await.unwrap();
        assert_eq!(backing.id, "yt:dEvrb6N70kA");
        assert!(f.fetched.lock().is_empty());
        assert_eq!(w.store.get_track("deezer:1").unwrap().status, "alias");
    }

    #[tokio::test]
    async fn youtube_track_downloads_its_own_video() {
        need!("ffmpeg");
        let w = World::new().await;
        w.remember("yt:sO4pSj466Hk", json!({"title": "Кошмар", "artists": ["ЮПИ"], "duration": 30}));
        let f = fetcher(vec![], &[]);
        let track = w.pipeline(f.clone()).download("yt:sO4pSj466Hk", &CancellationToken::new()).await.unwrap();
        assert_eq!((f.fetched.lock().clone(), *f.finds.lock()), (vec!["sO4pSj466Hk".to_string()], 0));
        assert!(track.file.unwrap().ends_with("ЮПИ/YouTube/Кошмар [yt-sO4pSj466Hk].m4a"));
    }

    #[tokio::test]
    async fn cancellation_stops_without_trying_other_candidates() {
        let w = World::new().await;
        w.remember("deezer:1", hella());
        struct Cancelling;
        impl Fetcher for Cancelling {
            fn find<'a>(&'a self, _: &'a Wanted, _: &'a HashSet<String>) -> BoxFuture<'a, Vec<Candidate>> {
                Box::pin(async { vec![cand("AAAAAAAAAAA", "youtube"), cand("BBBBBBBBBBB", "youtube")] })
            }
            fn download<'a>(&'a self, _: &'a Candidate, _: &'a Path, _: Duration, _: &'a CancellationToken) -> BoxFuture<'a, Result<PathBuf, SourceError>> {
                Box::pin(async { Err(SourceError::Cancelled) })
            }
        }
        let mut p = w.pipeline(fetcher(vec![], &[]));
        p.fetcher = Arc::new(Cancelling);
        assert!(matches!(p.download("deezer:1", &CancellationToken::new()).await, Err(SourceError::Cancelled)));
    }
}

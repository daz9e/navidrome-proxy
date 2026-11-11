//! The music directory and its reflection in Navidrome.
//!
//! Layout (readable without the proxy, usable by Plex/Jellyfin and folder browsing):
//!
//! ```text
//! <album artist>/<album>/<disc>-<track> - <title> [deezer-<id>].m4a
//! <artist>/YouTube/<title> [yt-<video id>].m4a
//! ```
//!
//! The `[deezer-<id>]` token keeps the file tied to its catalog entry, and the
//! mapping can be rebuilt from the files alone. Files of the first proxy generation
//! (flat `deezer-<id>.m4a`) are recognized too.
//!
//! Identity rule: an external id (`deezer:<id>`) is only the *primary* id while a
//! track is not in the library. Once Navidrome has indexed it, the Navidrome song id
//! is what the proxy lists everywhere; the external id stays a permanent alias.

use crate::db::{Field, Store, Track};
use crate::ids::{self, Kind};
use crate::matching;
use crate::navidrome::Navidrome;
use crate::subsonic::params::Params;
use crate::util::{Json, as_f64, as_str, as_uint, truthy};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use unicode_normalization::UnicodeNormalization;

static TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[(deezer-\d+|yt-[A-Za-z0-9_-]{11}|sc-\d+)\]").unwrap());
static LEGACY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(deezer-\d+|yt-[A-Za-z0-9_-]{11})$").unwrap());
/// First proxy: a Deezer track whose audio came from YouTube, kept without album tags.
pub static LEGACY_YOUTUBE_FALLBACK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^yt-(\d+)-yt$").unwrap());
static UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"[\x00-\x1f/\\:*?"<>|]+"#).unwrap());

/// Byte cap per path component: leaves room for the track prefix, the id token, the
/// suffix and publish's `.<name>.<hex>.part` temp name within ext4's 255 bytes.
const MAX_NAME_BYTES: usize = 180;

/// The id token embedded in file names: `deezer-<id>`, `yt-<video id>` or `sc-<id>`.
pub fn file_stem(track_id: &str) -> Option<String> {
    let vid = ids::parse(track_id).filter(|v| v.is_track())?;
    let prefix = match vid.kind {
        Kind::Track => "deezer",
        Kind::Youtube => "yt",
        _ => "sc",
    };
    Some(format!("{prefix}-{}", vid.key))
}

/// Recover the external track id from a library file name (new or legacy).
pub fn track_id_from_path(path: &str) -> Option<String> {
    let stem = Path::new(path).file_stem()?.to_str()?;
    if let Some(c) = LEGACY_YOUTUBE_FALLBACK.captures(stem) {
        return Some(format!("deezer:{}", &c[1]));
    }
    let token = TOKEN.captures(stem).or_else(|| LEGACY.captures(stem))?;
    let (prefix, key) = token[1].split_once('-')?;
    Some(format!("{prefix}:{key}"))
}

pub fn safe_name(value: &str, limit: usize, fallback: &str) -> String {
    let text: String = value.nfc().collect::<String>();
    let text = UNSAFE.replace_all(text.trim(), " ");
    let mut text = text.split_whitespace().collect::<Vec<_>>().join(" ").trim_matches(['.', ' ']).to_string();
    if text.len() > limit * 2 || text.chars().count() > limit {
        text = text.chars().take(limit).collect::<String>().trim_end_matches(['.', ' ']).to_string();
    }
    if text.len() > MAX_NAME_BYTES {
        let mut cut = MAX_NAME_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text = text[..cut].trim_end_matches(['.', ' ']).to_string();
    }
    if text.is_empty() { fallback.to_string() } else { text }
}

/// `<library>`-relative path of a track file.
pub fn relative_path(track_id: &str, meta: &Json, suffix: &str) -> Option<PathBuf> {
    let token = file_stem(track_id)?;
    let suffix = suffix.trim_start_matches('.');
    let title = safe_name(&as_str(meta.get("title")), 100, "Untitled");
    let lead = match meta.get("artists").and_then(Value::as_array).and_then(|a| a.first()) {
        Some(first) => as_str(Some(first)),
        None => as_str(meta.get("artist")),
    };
    if !token.starts_with("deezer-") && !truthy(meta.get("album")) {
        let folder = if token.starts_with("yt-") { "YouTube" } else { "SoundCloud" };
        return Some(PathBuf::from(safe_name(&lead, 80, "Unknown")).join(folder).join(format!("{title} [{token}].{suffix}")));
    }
    let album_artist = Some(as_str(meta.get("album_artist"))).filter(|s| !s.is_empty()).unwrap_or(lead);
    let folder = PathBuf::from(safe_name(&album_artist, 80, "Unknown")).join(safe_name(&as_str(meta.get("album")), 80, "Singles"));
    let track = as_uint(meta.get("track"));
    let prefix = if track > 0 {
        let disc = match as_uint(meta.get("disc")) {
            0 => 1,
            d => d,
        };
        format!("{disc}-{track:02} - ")
    } else {
        String::new()
    };
    Some(folder.join(format!("{prefix}{title} [{token}].{suffix}")))
}

fn short_hex(len: usize) -> String {
    format!("{:016x}", rand::random::<u64>())[..len].to_string()
}

pub struct Library {
    pub music_dir: PathBuf,
    pub staging_dir: PathBuf,
}

impl Library {
    pub fn new(music_dir: impl Into<PathBuf>, staging_dir: impl Into<PathBuf>) -> Self {
        Self { music_dir: music_dir.into(), staging_dir: staging_dir.into() }
    }

    pub fn new_staging(&self, track_id: &str) -> io::Result<PathBuf> {
        let stem = file_stem(track_id).ok_or_else(|| invalid(track_id))?;
        let path = self.staging_dir.join(format!("{stem}-{}", short_hex(8)));
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    /// Remove leftovers of crashed downloads older than `max_age` seconds.
    pub fn cleanup_staging(&self, max_age: f64) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.staging_dir) else { return 0 };
        let cutoff = crate::util::now() - max_age;
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            if mtime < cutoff {
                let result = if meta.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                if result.is_ok() {
                    removed += 1;
                }
            }
        }
        removed
    }

    /// Whether `path` (not following a final symlink) lies inside the music directory.
    pub fn contains(&self, path: &Path) -> bool {
        let (Ok(root), Some(parent), Some(name)) = (self.music_dir.canonicalize(), path.parent(), path.file_name()) else {
            return false;
        };
        parent.canonicalize().is_ok_and(|p| p.join(name).starts_with(&root) && p.join(name) != root)
    }

    /// Atomically move a finished file into the library.
    ///
    /// Replacing an existing file (source upgrade) is a single rename, so a
    /// concurrent stream keeps reading the old inode and never sees a partial file.
    /// `previous` is only deleted when it lies inside the music directory.
    pub fn publish(&self, track_id: &str, prepared: &Path, meta: &Json, previous: Option<&str>) -> io::Result<PathBuf> {
        let suffix = crate::audio::suffix_of(prepared);
        let dest = self.music_dir.join(relative_path(track_id, meta, &suffix).ok_or_else(|| invalid(track_id))?);
        let parent = dest.parent().unwrap_or(&self.music_dir);
        std::fs::create_dir_all(parent)?;
        let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("track");
        let tmp = dest.with_file_name(format!(".{name}.{}.part", short_hex(6)));
        // A rename fails across filesystems (staging on another mount): copy instead.
        if std::fs::rename(prepared, &tmp).is_err() {
            std::fs::copy(prepared, &tmp)?;
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Some(previous) = previous.map(PathBuf::from).filter(|p| *p != dest) {
            if !self.contains(&previous) {
                tracing::warn!("not deleting {}: outside the music directory", previous.display());
            } else if std::fs::remove_file(&previous).is_ok() {
                for ext in ["lrc", "txt"] {
                    let (old, new) = (previous.with_extension(ext), dest.with_extension(ext));
                    if old.exists() && !new.exists() {
                        let _ = std::fs::rename(&old, &new);
                    }
                }
                if let Some(folder) = previous.parent() {
                    self.prune(folder);
                }
            }
        }
        Ok(dest)
    }

    /// Remove folders left empty by a move, never the music root itself.
    pub fn prune(&self, folder: &Path) {
        let (Ok(root), Ok(mut current)) = (self.music_dir.canonicalize(), folder.canonicalize()) else { return };
        while current != root && current.starts_with(&root) {
            if std::fs::remove_dir(&current).is_err() {
                return;
            }
            match current.parent() {
                Some(p) => current = p.to_path_buf(),
                None => return,
            }
        }
    }
}

fn invalid(track_id: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("not an external track id: {track_id}"))
}

pub type CredentialsProvider = Arc<dyn Fn() -> Option<Params> + Send + Sync>;
pub type OnMapped = Arc<dyn Fn(Track) -> BoxFuture<'static, ()> + Send + Sync>;

/// While tracks wait for a song id, scan again this often even without a new
/// publish: a refused `startScan` or a watcher that missed a file must not strand them.
const RESCAN: Duration = Duration::from_secs(120);

/// Gets new files into Navidrome and learns their song ids.
///
/// Scans are debounced (one `startScan` for a burst of imports) and song ids are
/// resolved with a targeted `search3`.
pub struct Indexer {
    store: Arc<Store>,
    navidrome: Arc<Navidrome>,
    credentials: CredentialsProvider,
    debounce: Duration,
    /// Pause between `startScan` and the first lookup.
    pub settle: Duration,
    on_mapped: Option<OnMapped>,
    wake: Notify,
    pending_since: Mutex<Option<Instant>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Indexer {
    pub fn new(store: Arc<Store>, navidrome: Arc<Navidrome>, credentials: CredentialsProvider, debounce: f64, on_mapped: Option<OnMapped>) -> Self {
        Self {
            store,
            navidrome,
            credentials,
            debounce: Duration::from_secs_f64(debounce.max(0.0)),
            settle: Duration::from_secs(1),
            on_mapped,
            wake: Notify::new(),
            pending_since: Mutex::new(None),
            task: Mutex::new(None),
        }
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        *self.task.lock() = Some(tokio::spawn(async move { me.run().await }));
    }

    pub async fn stop(&self) {
        let task = self.task.lock().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    pub fn notify(&self) {
        self.pending_since.lock().get_or_insert_with(Instant::now);
        self.wake.notify_one();
    }

    async fn run(&self) {
        let mut backoff = Duration::from_secs(5);
        let mut last_scan: Option<Instant> = None;
        loop {
            let _ = tokio::time::timeout(backoff, self.wake.notified()).await;
            backoff = self.iteration(&mut last_scan).await;
        }
    }

    /// One pass; returns how long to sleep before the next one.
    async fn iteration(&self, last_scan: &mut Option<Instant>) -> Duration {
        let pending = self.store.ready_without_local_id(200);
        if pending.is_empty() {
            *self.pending_since.lock() = None;
            return Duration::from_secs(60);
        }
        let since = self.pending_since.lock().take();
        if since.is_some() || last_scan.is_none_or(|t| t.elapsed() >= RESCAN) {
            if let Some(since) = since {
                tokio::time::sleep(self.debounce.saturating_sub(since.elapsed())).await;
            }
            self.scan().await;
            *last_scan = Some(Instant::now());
            tokio::time::sleep(self.settle).await;
        }
        let resolved = self.resolve(&pending).await;
        // Poll fast while files are being indexed; give up quickly on files
        // Navidrome refuses (a later notify() or rescan retries them).
        let now = crate::util::now();
        if pending.iter().all(|t| now - t.updated_at > 1800.0) && resolved == 0 {
            Duration::from_secs(300)
        } else if resolved < pending.len() {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(5)
        }
    }

    pub async fn scan(&self) {
        let Some(creds) = (self.credentials)() else { return };
        match self.navidrome.call("startScan", &creds, &[]).await {
            Ok(_) => {}
            // Non-admin users cannot start scans; Navidrome's watcher still picks files up.
            Err(e @ crate::navidrome::NdError::Upstream { .. }) => tracing::debug!("startScan refused: {e}"),
            Err(e) => tracing::warn!("startScan failed: {e}"),
        }
    }

    pub async fn resolve(&self, tracks: &[Track]) -> usize {
        let Some(creds) = (self.credentials)() else { return 0 };
        let mut resolved = 0;
        for track in tracks {
            let Some(local_id) = self.find_local_id(track, &creds).await else { continue };
            self.store.update_track(&track.id, &[Field::LocalId(Some(local_id))]);
            resolved += 1;
            if let (Some(hook), Some(fresh)) = (&self.on_mapped, self.store.get_track(&track.id)) {
                hook(fresh).await;
            }
        }
        resolved
    }

    /// Find the Navidrome song of a downloaded file.
    ///
    /// Navidrome does not report real file paths by default (`path` is rebuilt from
    /// tags), so the file name cannot be used. The exact byte size of the file we
    /// wrote is: title + size identifies the song unambiguously.
    pub async fn find_local_id(&self, track: &Track, creds: &Params) -> Option<String> {
        let file = track.file.as_deref().filter(|f| !f.is_empty())?;
        let size = std::fs::metadata(file).ok()?.len() as i64;
        let token = file_stem(&track.id)?;
        let file_name = Path::new(file).file_name().and_then(|n| n.to_str()).unwrap_or("");
        let title = track.title();
        let mut queries = vec![title.clone()];
        let with_artist = format!("{title} {}", track.artists().first().cloned().unwrap_or_default()).trim().to_string();
        if with_artist != title {
            queries.push(with_artist);
        }
        let duration = as_f64(track.audio.get("duration")).filter(|d| *d != 0.0).unwrap_or_else(|| track.duration());
        for query in queries.into_iter().filter(|q| !q.is_empty()) {
            let root = match self.navidrome.call("search3", creds, &[("query", &query), ("songCount", "100"), ("albumCount", "0"), ("artistCount", "0")]).await {
                Ok(root) => root,
                Err(e) => {
                    tracing::debug!("local id lookup failed for {}: {e}", track.id);
                    return None;
                }
            };
            let songs = crate::navidrome::songs_of(root.get("searchResult3"));
            let size_of = |s: &Json| as_f64(s.get("size")).map(|f| f as i64).unwrap_or(-1);
            let by_size: Vec<&Json> = songs.iter().filter(|s| size_of(s) == size).collect();
            if by_size.len() == 1 {
                return Some(crate::navidrome::id_of(by_size[0]));
            }
            // Real paths are reported when the server is configured to do so.
            for song in &songs {
                let path = as_str(song.get("path"));
                let base = path.rsplit('/').next().unwrap_or("");
                if base.contains(&format!("[{token}]")) || base == file_name {
                    return Some(crate::navidrome::id_of(song));
                }
            }
            let pool: Vec<&Json> = if by_size.is_empty() { songs.iter().collect() } else { by_size };
            let (want_title, want_album) = (matching::normalize(&title), matching::normalize(&as_str(track.meta.get("album"))));
            let exact: Vec<&&Json> = pool
                .iter()
                .filter(|s| {
                    matching::normalize(&as_str(s.get("title"))) == want_title
                        && matching::normalize(&as_str(s.get("album"))) == want_album
                        && (as_f64(s.get("duration")).unwrap_or(0.0) - duration).abs() <= 2.0
                })
                .collect();
            if exact.len() == 1 {
                return Some(crate::navidrome::id_of(exact[0]));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeNavidrome, m, nd_song, serve};
    use serde_json::json;

    fn meta() -> Json {
        m(json!({"title": "Hella Quality", "artists": ["TVETH", "Bato"], "album_artist": "TVETH", "album": "RAW", "track": 3, "disc": 1}))
    }

    #[test]
    fn file_stem_kinds() {
        assert_eq!(file_stem("deezer:42").as_deref(), Some("deezer-42"));
        assert_eq!(file_stem("yt:sO4pSj466Hk").as_deref(), Some("yt-sO4pSj466Hk"));
        assert_eq!(file_stem("sc:123").as_deref(), Some("sc-123"));
        assert_eq!(file_stem("L1"), None);
        assert_eq!(file_stem("deezer-album:1"), None);
    }

    #[test]
    fn relative_path_layout() {
        let p = |id: &str, meta: Value, sfx: &str| relative_path(id, &m(meta), sfx).unwrap().display().to_string();
        assert_eq!(relative_path("deezer:1", &meta(), ".m4a").unwrap().display().to_string(), "TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a");
        assert_eq!(p("yt:sO4pSj466Hk", json!({"title": "Кошмар", "artists": ["ЮПИ"]}), "m4a"), "ЮПИ/YouTube/Кошмар [yt-sO4pSj466Hk].m4a");
        assert_eq!(p("deezer:2", json!({"title": "AC/DC: Why?", "artist": "..", "album": null}), ".mp3"), "Unknown/Singles/AC DC Why [deezer-2].mp3");
        assert_eq!(p("sc:5", json!({"title": "x", "artist": "Y"}), "m4a"), "Y/SoundCloud/x [sc-5].m4a");
        assert_eq!(p("yt:sO4pSj466Hk", json!({"title": "t", "artist": "A", "album": "Al", "track": 2, "disc": 0}), "m4a"), "A/Al/1-02 - t [yt-sO4pSj466Hk].m4a");
        assert!(relative_path("L1", &meta(), "m4a").is_none());
    }

    #[test]
    fn safe_name_limits_length() {
        assert_eq!(safe_name(&"x".repeat(500), 80, "Unknown").chars().count(), 80);
        assert_eq!(safe_name("  ", 80, "Unknown"), "Unknown");
        assert_eq!(safe_name("e\u{301}", 80, "U"), "\u{e9}", "NFC");
        // B9: 4-byte characters within the char limit still fit a file name.
        let emoji = safe_name(&"😀".repeat(100), 100, "U");
        assert!(emoji.len() <= MAX_NAME_BYTES && emoji.chars().all(|c| c == '😀'));
        let path = relative_path("deezer:12345678901234567890", &m(json!({"title": "😀".repeat(100), "album": "a", "track": 99, "disc": 99})), "m4a").unwrap();
        assert!(path.file_name().unwrap().len() + ".x.abcdef.part".len() <= 255);
    }

    #[test]
    fn track_id_from_path_new_and_legacy() {
        let t = track_id_from_path;
        assert_eq!(t("TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a").as_deref(), Some("deezer:1"));
        assert_eq!(t("/music/deezer-4274804272.m4a").as_deref(), Some("deezer:4274804272"));
        assert_eq!(t("ЮПИ/YouTube/Кошмар [yt-sO4pSj466Hk].m4a").as_deref(), Some("yt:sO4pSj466Hk"));
        assert_eq!(t("Юпи/SoundCloud/x [sc-2398309779].m4a").as_deref(), Some("sc:2398309779"));
        assert_eq!(t("Persona/Aria of the Soul.m4a"), None);
        // first proxy: YouTube audio for a Deezer track ("99624046-yt" is 11 chars, not a video id)
        assert_eq!(t("/music/yt-99624046-yt.m4a").as_deref(), Some("deezer:99624046"));
        assert_eq!(t("/music/yt-4145181542-yt.m4a").as_deref(), Some("deezer:4145181542"));
    }

    #[test]
    fn publish_is_atomic_and_moves_sidecars_on_format_change() {
        let tmp = tempfile::tempdir().unwrap();
        let music = tmp.path().join("music");
        let lib = Library::new(&music, music.join(".staging"));
        let first = lib.new_staging("deezer:1").unwrap().join("audio.m4a");
        std::fs::write(&first, b"one").unwrap();
        let dest = lib.publish("deezer:1", &first, &meta(), None).unwrap();
        assert_eq!(dest, music.join("TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"one");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::write(dest.with_extension("lrc"), "[00:01]x").unwrap();
        let upgrade = lib.new_staging("deezer:1").unwrap().join("audio.mp3");
        std::fs::write(&upgrade, b"two").unwrap();
        let dest2 = lib.publish("deezer:1", &upgrade, &meta(), Some(dest.to_str().unwrap())).unwrap();
        assert!(dest2.extension().unwrap() == "mp3" && !dest.exists());
        assert_eq!(std::fs::read_to_string(dest2.with_extension("lrc")).unwrap(), "[00:01]x");
        let parts: Vec<_> = walk(&music).into_iter().filter(|p| p.to_string_lossy().ends_with(".part")).collect();
        assert!(parts.is_empty());
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            }
            out.push(p);
        }
        out
    }

    #[test]
    fn publish_prunes_empty_folders_of_legacy_files() {
        let tmp = tempfile::tempdir().unwrap();
        let music = tmp.path().join("music");
        let lib = Library::new(&music, music.join(".staging"));
        let legacy_dir = music.join("Old/Folder");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let legacy = legacy_dir.join("deezer-1.m4a");
        std::fs::write(&legacy, b"old").unwrap();
        let new = lib.new_staging("deezer:1").unwrap().join("audio.m4a");
        std::fs::write(&new, b"new").unwrap();
        lib.publish("deezer:1", &new, &meta(), Some(legacy.to_str().unwrap())).unwrap();
        assert!(!music.join("Old").exists());
        assert!(music.is_dir());
    }

    #[test]
    fn publish_never_deletes_previous_outside_the_library() {
        let tmp = tempfile::tempdir().unwrap();
        let music = tmp.path().join("music");
        let lib = Library::new(&music, music.join(".staging"));
        let outside = tmp.path().join("precious.m4a");
        std::fs::write(&outside, b"keep").unwrap();
        let sneaky = music.join("../precious.m4a");
        let new = lib.new_staging("deezer:1").unwrap().join("audio.m4a");
        std::fs::write(&new, b"new").unwrap();
        lib.publish("deezer:1", &new, &meta(), Some(sneaky.to_str().unwrap())).unwrap();
        assert!(outside.exists());
        assert!(!lib.contains(&music), "the root itself is not inside");
        assert!(lib.publish("L1", &new, &meta(), None).is_err());
    }

    #[test]
    fn cleanup_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::new(tmp.path().join("m"), tmp.path().join("m/.staging"));
        assert_eq!(lib.cleanup_staging(0.0), 0, "missing dir");
        let old = lib.new_staging("deezer:1").unwrap();
        assert_eq!(lib.cleanup_staging(3600.0), 0, "fresh");
        assert_eq!(lib.cleanup_staging(-1.0), 1);
        assert!(!old.exists());
    }

    async fn env() -> (FakeNavidrome, Arc<Navidrome>, Arc<Store>, crate::testutil::Server, tempfile::TempDir) {
        let fake = FakeNavidrome::default();
        let server = serve(fake.router()).await;
        let nav = Arc::new(Navidrome::new(&server.url, 60.0));
        (fake, nav, Arc::new(Store::memory()), server, tempfile::tempdir().unwrap())
    }

    fn creds() -> CredentialsProvider {
        Arc::new(|| Some(Params::parse("u=alice&p=alicepw", "")))
    }

    fn sized(id: &str, title: &str, size: u64) -> Json {
        let mut s = nd_song(id, title, "A", "Album", "al1");
        s.insert("size".into(), json!(size));
        s.insert("path".into(), json!("A/Album/01-01 - Same Title.m4a"));
        s
    }

    fn ready(store: &Store, dir: &Path, i: u64, size: usize) -> PathBuf {
        let f = dir.join(format!("deezer-{i}.m4a"));
        std::fs::write(&f, vec![b'x'; size]).unwrap();
        store.remember_track(&format!("deezer:{i}"), &m(json!({"title": "Same Title", "artists": ["A"], "duration": 200})));
        store.update_track(&format!("deezer:{i}"), &[Field::status("ready"), Field::File(Some(f.display().to_string()))]);
        f
    }

    #[tokio::test]
    async fn indexer_scans_once_and_maps_by_file_size() {
        let (fake, nav, store, _s, dir) = env().await;
        for i in [1, 2] {
            ready(&store, dir.path(), i, 1000 + i as usize);
        }
        // Same title and a synthetic path (Navidrome hides real paths): the exact
        // byte size is what tells the two files apart.
        fake.state.lock().on_scan = Some(Arc::new(|s| {
            s.songs.push(sized("L1", "Same Title", 1001));
            s.songs.push(sized("L2", "Same Title", 1002));
        }));
        let mapped = Arc::new(Mutex::new(vec![]));
        let sink = mapped.clone();
        let hook: OnMapped = Arc::new(move |t: Track| {
            sink.lock().push((t.id, t.local_id.unwrap_or_default()));
            Box::pin(async {})
        });
        let mut indexer = Indexer::new(store.clone(), nav, creds(), 0.05, Some(hook));
        indexer.settle = Duration::from_millis(10);
        let indexer = Arc::new(indexer);
        indexer.start();
        indexer.notify();
        indexer.notify();
        for _ in 0..100 {
            if mapped.lock().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        indexer.stop().await;
        let mut got = mapped.lock().clone();
        got.sort();
        assert_eq!(got, [("deezer:1".to_string(), "L1".to_string()), ("deezer:2".into(), "L2".into())]);
        assert_eq!(fake.state.lock().scans, 1);
    }

    #[tokio::test]
    async fn rescans_without_notify_when_tracks_stay_unmapped() {
        // B4: a scan that missed the file is repeated even if nothing calls notify().
        let (fake, nav, store, _s, dir) = env().await;
        ready(&store, dir.path(), 1, 1001);
        let mut indexer = Indexer::new(store.clone(), nav, creds(), 0.0, None);
        indexer.settle = Duration::ZERO;
        let mut last_scan = None;
        indexer.iteration(&mut last_scan).await;
        assert_eq!(fake.state.lock().scans, 1, "first pass scans");
        indexer.iteration(&mut last_scan).await;
        assert_eq!(fake.state.lock().scans, 1, "not again right away");
        last_scan = Some(Instant::now() - RESCAN);
        fake.add_song(sized("L1", "Same Title", 1001));
        indexer.iteration(&mut last_scan).await;
        assert_eq!(fake.state.lock().scans, 2);
        assert_eq!(store.get_track("deezer:1").unwrap().local_id.as_deref(), Some("L1"));
        assert_eq!(indexer.iteration(&mut last_scan).await, Duration::from_secs(60), "idle");
    }

    #[tokio::test]
    async fn find_local_id_by_path_token_and_by_title() {
        let (fake, nav, store, _s, dir) = env().await;
        let f = ready(&store, dir.path(), 1, 10);
        let mut by_path = nd_song("P1", "Same Title", "A", "Album", "al1");
        by_path.insert("path".into(), json!("A/x [deezer-1].m4a"));
        fake.add_song(by_path);
        fake.add_song(nd_song("P2", "Same Title", "A", "Other", "al2"));
        let indexer = Indexer::new(store.clone(), nav.clone(), creds(), 0.0, None);
        let p = Params::parse("u=alice&p=alicepw", "");
        let track = store.get_track("deezer:1").unwrap();
        assert_eq!(indexer.find_local_id(&track, &p).await.as_deref(), Some("P1"));
        // No size or path hit: title + album + duration must be unique.
        fake.state.lock().songs.clear();
        fake.add_song(nd_song("P3", "Same Title", "A", "Album", "al1"));
        fake.add_song(nd_song("P4", "Same Title", "A", "Other", "al2"));
        store.remember_track("deezer:1", &m(json!({"title": "Same Title", "artists": ["A"], "album": "Album", "duration": 200})));
        let track = store.get_track("deezer:1").unwrap();
        assert_eq!(indexer.find_local_id(&track, &p).await.as_deref(), Some("P3"));
        std::fs::remove_file(f).unwrap();
        assert_eq!(indexer.find_local_id(&track, &p).await, None, "file gone");
    }

    #[tokio::test]
    async fn find_local_id_without_credentials_is_none() {
        let (_fake, nav, store, _s, _dir) = env().await;
        store.remember_track("deezer:1", &m(json!({"title": "x"})));
        store.update_track("deezer:1", &[Field::status("ready"), Field::File(Some("/music/deezer-1.m4a".into()))]);
        let indexer = Indexer::new(store.clone(), nav, Arc::new(|| None), 0.0, None);
        assert_eq!(indexer.resolve(&store.ready_without_local_id(200)).await, 0);
        indexer.scan().await;
    }
}

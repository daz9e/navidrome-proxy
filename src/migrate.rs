//! Maintenance commands: adopt the first proxy's library and state, relayout, remux.
//!
//! `migrate-legacy` is idempotent and never touches audio files:
//!
//! 1. every library file named with an id token (`deezer-<id>.m4a` or
//!    `... [deezer-<id>].m4a`) becomes a ready track (tags + ffprobe);
//! 2. the legacy `state.json` adds Navidrome ids, catalog metadata and per-user
//!    pending likes; liked tracks that never finished downloading are queued again;
//! 3. pending likes of catalog-less legacy imports (`fallback:*`) become a new
//!    import for that user, so the better matcher gets a second try.

use crate::audio::{AudioInfo, Tools, suffix_of};
use crate::covers::{Covers, is_complete_image};
use crate::db::{Field, Store};
use crate::deezer::Deezer;
use crate::identity::Identity;
use crate::ids;
use crate::library::{LEGACY_YOUTUBE_FALLBACK, Library, files_under, relative_path, track_id_from_path};
use crate::matching;
use crate::pipeline::cover_for_tags;
use crate::util::{Json, as_f64, as_str, opt_str, truthy};
use futures::StreamExt;
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const AUDIO: [&str; 5] = ["m4a", "mp3", "ogg", "opus", "flac"];
const STAGING: &str = ".navidrome-proxy-staging";

fn bump(stats: &mut Json, key: &str, by: i64) {
    let n = stats.get(key).and_then(Value::as_i64).unwrap_or(0);
    stats.insert(key.into(), json!(n + by));
}

fn zeroed(keys: &[&str]) -> Json {
    keys.iter().map(|k| (k.to_string(), json!(0))).collect()
}

/// Library files with one of `suffixes`, outside the staging folder.
fn library_files(music_dir: &Path, suffixes: &[&str]) -> Vec<PathBuf> {
    files_under(music_dir)
        .into_iter()
        .filter(|p| suffixes.contains(&suffix_of(p).as_str()) && !p.components().any(|c| c.as_os_str() == STAGING))
        .collect()
}

fn tags_meta(path: &Path) -> Json {
    let tags = crate::tagging::read_tags(path);
    let first = |key: &str| tags.get(key).and_then(|v| v.first()).cloned();
    let artist = first("artist").unwrap_or_default();
    let mut artists = matching::split_artists(&artist.replace(" / ", ", "));
    if artists.is_empty() && !artist.is_empty() {
        artists = vec![artist.clone()];
    }
    let track = first("tracknumber").unwrap_or_default();
    let track = track.split('/').next().unwrap_or("");
    let year: String = first("date").unwrap_or_default().chars().take(4).collect();
    let joined = artists.join(", ");
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let v = json!({
        "title": first("title").unwrap_or_else(|| stem.to_string()),
        "artist": if joined.is_empty() { artist } else { joined },
        "artists": artists,
        "album": first("album"),
        "album_artist": first("albumartist"),
        "track": if !track.is_empty() && track.chars().all(|c| c.is_ascii_digit()) { track.parse::<i64>().ok() } else { None },
        "year": if year.is_empty() { None } else { Some(year) },
        "genre": first("genre"),
    });
    v.as_object().cloned().unwrap_or_default()
}

fn legacy_meta(job: &Json) -> Json {
    let mut artists: Vec<String> = job.get("artists").and_then(Value::as_array).into_iter().flatten().map(|v| as_str(Some(v))).collect();
    if artists.is_empty() {
        artists = matching::split_artists(&as_str(job.get("artist")).replace(" / ", ", "));
    }
    let id_str = |key: &str| truthy(job.get(key)).then(|| as_str(job.get(key)));
    let mut meta = json!({
        "title": job.get("title"),
        "artists": artists,
        "artist": if artists.is_empty() { job.get("artist").cloned().unwrap_or(Value::Null) } else { json!(artists.join(", ")) },
        "album": job.get("album"),
        "album_id": id_str("album_id"),
        "artist_id": id_str("artist_id"),
        "duration": job.get("duration"),
        "cover": job.get("cover"),
    });
    let id = as_str(job.get("id"));
    if let Some(key) = id.strip_prefix("deezer:") {
        meta["source"] = json!("deezer");
        meta["deezer_id"] = json!(key);
    }
    let empty = |v: &Value| v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(Vec::is_empty);
    meta.as_object().cloned().unwrap_or_default().into_iter().filter(|(_, v)| !empty(v)).collect()
}

/// Make `path` the ready file of `track_id`; returns the stats key of the outcome.
async fn adopt(store: &Store, tools: &Tools, track_id: &str, path: &Path) -> &'static str {
    let existing = store.get_track(track_id);
    let file = path.display().to_string();
    if existing.as_ref().is_some_and(|t| t.is_ready() && t.file.as_deref() == Some(file.as_str())) {
        return "files_known";
    }
    let info = match tools.probe(path).await {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!("cannot read {}: {e}", path.display());
            return "files_unreadable";
        }
    };
    let owned = path.to_path_buf();
    let Ok(mut meta) = tokio::task::spawn_blocking(move || tags_meta(&owned)).await else { return "files_unreadable" };
    meta.insert("duration".into(), json!(info.duration.round() as i64));
    if let Some(t) = &existing {
        meta.retain(|k, _| !t.meta.contains_key(k));
    }
    store.remember_track(track_id, &meta);
    let source = existing.and_then(|t| t.source).unwrap_or_else(|| "legacy".into());
    store.update_track(
        track_id,
        &[Field::status("ready"), Field::File(Some(file)), Field::Audio(Some(info.as_json())), Field::Source(Some(source)), Field::Error(None)],
    );
    "files_adopted"
}

pub async fn migrate_legacy(store: &Arc<Store>, tools: &Tools, music_dir: &Path, state_file: Option<&Path>, legacy_music_root: &str) -> Json {
    let mut stats = Json::new();
    let identity = Identity::new(store.clone());

    // 1. Files already in the library.
    let files: Vec<(String, PathBuf)> = library_files(music_dir, &AUDIO)
        .into_iter()
        .filter_map(|p| Some((track_id_from_path(p.to_str()?)?, p)))
        .filter(|(id, _)| ids::parse(id).is_some())
        .collect();
    let outcomes: Vec<&str> = futures::stream::iter(files.iter().map(|(id, p)| adopt(store, tools, id, p))).buffer_unordered(4).collect().await;
    for key in outcomes {
        bump(&mut stats, key, 1);
    }

    let Some(state_file) = state_file.filter(|f| f.is_file()) else { return stats };
    // 2. Legacy state.
    let state: Value = match std::fs::read_to_string(state_file).map(|s| serde_json::from_str(&s)) {
        Ok(Ok(v)) => v,
        _ => {
            tracing::warn!("cannot read {}", state_file.display());
            return stats;
        }
    };
    let library = Library::new(music_dir, music_dir.join(STAGING));
    let mut fallback_likes: Vec<(String, Vec<String>)> = Vec::new();
    for (key, job) in state.get("jobs").and_then(Value::as_object).into_iter().flatten() {
        let Some(job) = job.as_object() else { continue };
        let pending: Vec<String> = job.get("pending_star_users").and_then(Value::as_object).into_iter().flatten().map(|(u, _)| u.clone()).collect();
        let mut users = pending.clone();
        for reason in job.get("storage_reasons").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if let Some(user) = reason.strip_prefix("star:")
                && !users.iter().any(|u| u == user)
            {
                users.push(user.into());
            }
        }
        if key.starts_with("fallback:") {
            let (artist, title) = (as_str(job.get("artist")), as_str(job.get("title")));
            if as_str(job.get("status")) != "ready" && !artist.is_empty() && !title.is_empty() {
                for user in users {
                    let line = format!("{artist} - {title}");
                    match fallback_likes.iter_mut().find(|(u, _)| *u == user) {
                        Some((_, lines)) if !lines.contains(&line) => lines.push(line),
                        Some(_) => {}
                        None => fallback_likes.push((user, vec![line])),
                    }
                }
            }
            continue;
        }
        if !ids::parse(key).is_some_and(|v| v.is_track()) {
            continue;
        }
        store.remember_track(key, &legacy_meta(job));
        bump(&mut stats, "state_tracks", 1);
        let track = store.get_track(key).unwrap_or_default();
        if truthy(job.get("local_id")) && track.is_ready() && track.local_id.is_none() {
            store.update_track(key, &[Field::LocalId(Some(as_str(job.get("local_id"))))]);
            bump(&mut stats, "local_ids", 1);
        }
        if as_str(job.get("status")) == "ready" && !track.is_ready() && let Some(file) = opt_str(job.get("file")).filter(|f| !f.is_empty()) {
            let candidate = match Path::new(&file).strip_prefix(legacy_music_root) {
                Ok(rel) => music_dir.join(rel),
                _ => PathBuf::from(&file),
            };
            // B3: state.json is not trusted to name files outside the library.
            if !library.contains(&candidate) {
                tracing::warn!("{key}: {} is outside the music directory; not adopted", candidate.display());
            } else if candidate.is_file() {
                let outcome = adopt(store, tools, key, &candidate).await;
                bump(&mut stats, outcome, 1);
            }
        }
        for user in &pending {
            store.add_pending_star(user, key);
            bump(&mut stats, "pending_likes", 1);
        }
        let track = store.get_track(key);
        if let (Some(user), Some(t)) = (pending.first(), &track)
            && !t.is_ready()
        {
            store.enqueue(key, "download", 2, Some(&format!("like:{user}")), false);
            bump(&mut stats, "requeued_likes", 1);
        }
        if let Some(t) = track.filter(|t| t.is_ready()) {
            identity.register(&t);
        }
    }
    for (user, lines) in fallback_likes {
        let Ok(rows) = crate::imports::parse_text(&lines.join("\n")) else { continue };
        let import_id = format!("legacy-{}", user.to_lowercase());
        if store.get_import(&import_id).is_none() {
            let rows_v = Value::Array(rows.iter().cloned().map(Value::Object).collect());
            store.save_import(&import_id, &user, "queued", &json!({"like": true, "playlist": null, "playlist_id": null}), &rows_v);
            bump(&mut stats, "legacy_import_rows", rows.len() as i64);
        }
    }
    stats
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(x), Ok(y)) if x.dev() == y.dev() && x.ino() == y.ino())
}

/// Move adopted files into the `Artist/Album/NN - Title [id].ext` layout.
///
/// Never overwrites: a target that is another file is reported as `skipped`
/// (B5/B6). The move is a hard link + DB update + unlink, so an interrupted run
/// is completed by the next one.
pub fn relayout(store: &Store, music_dir: &Path, dry_run: bool) -> Json {
    let library = Library::new(music_dir, music_dir.join(STAGING));
    let mut stats = zeroed(&["moved", "kept", "missing", "skipped"]);
    for track in store.tracks_with_status(&["ready"]) {
        let Some(file) = track.file.clone().filter(|f| !f.is_empty()) else { continue };
        let current = PathBuf::from(&file);
        if !current.is_file() {
            bump(&mut stats, "missing", 1);
            continue;
        }
        let Some(rel) = relative_path(&track.id, &track.meta, &suffix_of(&current)) else {
            bump(&mut stats, "skipped", 1);
            continue;
        };
        let target = music_dir.join(rel);
        if target == current {
            bump(&mut stats, "kept", 1);
            continue;
        }
        if !library.contains(&current) || (target.exists() && !same_file(&current, &target)) {
            tracing::warn!("relayout {}: not moving {} to {}", track.id, current.display(), target.display());
            bump(&mut stats, "skipped", 1);
            continue;
        }
        bump(&mut stats, "moved", 1);
        if dry_run {
            continue;
        }
        if let Err(e) = move_file(&current, &target) {
            tracing::warn!("relayout {}: {e}", track.id);
            bump(&mut stats, "moved", -1);
            bump(&mut stats, "skipped", 1);
            continue;
        }
        store.update_track(&track.id, &[Field::File(Some(target.display().to_string()))]);
        let _ = std::fs::remove_file(&current);
        for ext in ["lrc", "txt"] {
            let (old, new) = (current.with_extension(ext), target.with_extension(ext));
            if old.exists() && !new.exists() {
                let _ = std::fs::rename(&old, &new);
            }
        }
        if let Some(parent) = current.parent() {
            library.prune(parent);
        }
    }
    stats
}

/// Make `target` the same file as `current` without clobbering anything.
fn move_file(current: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target.parent().unwrap_or(Path::new("/")))?;
    if same_file(current, target) {
        return Ok(());
    }
    match std::fs::hard_link(current, target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
        // Another filesystem (or no hard links): a plain copy keeps `current` until the DB is updated.
        Err(_) => {
            let tmp = target.with_extension("relayout.part");
            std::fs::copy(current, &tmp)?;
            std::fs::rename(&tmp, target)
        }
    }
}

/// Losslessly rewrite DASH-fragmented AAC files (wrong duration in AVFoundation).
pub async fn remux_dash(tools: &Tools, music_dir: &Path, dry_run: bool) -> Json {
    let mut paths = Vec::new();
    for p in library_files(music_dir, &["m4a"]) {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            // B8: temp files of an interrupted remux.
            if name.contains(".remux.") && !dry_run {
                let _ = std::fs::remove_file(&p);
            }
            continue;
        }
        paths.push(p);
    }
    let one = |path: PathBuf| async move {
        let header = match tools.probe(&path).await {
            Ok(info) => info,
            Err(e) => {
                tracing::warn!("remux of {} failed: {e}", path.display());
                return "failed";
            }
        };
        if dry_run {
            return if header.brand == "dash" { "remuxed" } else { "" };
        }
        match tools.remux_in_place(&path).await {
            Ok(false) => "",
            Ok(true) => {
                let real = tools.probe(&path).await.map(|i: AudioInfo| i.duration).unwrap_or(0.0);
                if header.duration > 0.0 && real < 0.8 * header.duration {
                    // The download stopped early; only the DASH header knew the full length.
                    tracing::warn!("{} is truncated ({real:.0}s of {:.0}s): download it again", path.display(), header.duration);
                    "truncated"
                } else {
                    "remuxed"
                }
            }
            Err(e) => {
                tracing::warn!("remux of {} failed: {e}", path.display());
                "failed"
            }
        }
    };
    let mut stats = zeroed(&["checked", "remuxed", "truncated", "failed"]);
    stats.insert("checked".into(), json!(paths.len()));
    let outcomes: Vec<&str> = futures::stream::iter(paths.into_iter().map(one)).buffer_unordered(2).collect().await;
    for key in outcomes {
        match key {
            "truncated" => {
                bump(&mut stats, "remuxed", 1);
                bump(&mut stats, "truncated", 1);
            }
            "" => {}
            k => bump(&mut stats, k, 1),
        }
    }
    stats
}

/// Re-embed artwork in files whose cover is truncated (or missing) from the catalog.
pub async fn repair_covers(store: &Store, tools: &Tools, music_dir: &Path, covers: &Covers, work_dir: &Path, dry_run: bool) -> Json {
    let mut stats = zeroed(&["checked", "broken", "repaired", "unresolved"]);
    for path in library_files(music_dir, &["m4a", "mp3"]) {
        bump(&mut stats, "checked", 1);
        let p = path.clone();
        let current = tokio::task::spawn_blocking(move || crate::tagging::embedded_cover(&p)).await.ok().flatten();
        if current.as_deref().is_some_and(is_complete_image) {
            continue;
        }
        bump(&mut stats, "broken", 1);
        let track_id = path.to_str().and_then(track_id_from_path);
        let mut url = track_id.as_deref().and_then(|t| store.get_track(t)).and_then(|t| opt_str(t.meta.get("cover"))).unwrap_or_default();
        if url.is_empty()
            && let Some(video) = track_id.as_deref().and_then(|t| t.strip_prefix("yt:"))
        {
            url = format!("https://i.ytimg.com/vi/{video}/hqdefault.jpg");
        }
        let _ = std::fs::create_dir_all(work_dir);
        let Some(cover) = cover_for_tags(covers, tools, &url, work_dir).await else {
            bump(&mut stats, "unresolved", 1);
            tracing::warn!("no cover for {}", path.display());
            continue;
        };
        if !dry_run {
            let p = path.clone();
            match tokio::task::spawn_blocking(move || crate::tagging::set_cover(&p, &cover)).await {
                Ok(Ok(())) => {}
                failed => {
                    tracing::warn!("cannot embed a cover in {}: {failed:?}", path.display());
                    bump(&mut stats, "unresolved", 1);
                    continue;
                }
            }
        }
        bump(&mut stats, "repaired", 1);
    }
    stats
}

/// Give the first proxy's `yt-<deezer id>-yt` files their catalog tags.
///
/// The first proxy stored YouTube audio for a Deezer track without album tags, so
/// such songs sat in "[Unknown Album]" without artwork. The tags are rewritten in
/// place, which Navidrome treats as the same song (id, likes, plays and playlists
/// stay). Run `relayout` only after Navidrome has rescanned: a retag and a move
/// seen in the same scan would give the song a new id.
#[allow(clippy::too_many_arguments)]
pub async fn adopt_youtube_fallbacks(
    store: &Arc<Store>,
    tools: &Tools,
    music_dir: &Path,
    deezer: &Deezer,
    covers: &Covers,
    work_dir: &Path,
    dry_run: bool,
    tolerance: f64,
) -> Json {
    let mut stats = Json::new();
    let identity = Identity::new(store.clone());
    for path in library_files(music_dir, &AUDIO) {
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let Some(key) = LEGACY_YOUTUBE_FALLBACK.captures(stem).map(|c| c[1].to_string()) else { continue };
        bump(&mut stats, "found", 1);
        let track_id = ids::track(&key);
        let file = path.display().to_string();
        let existing = store.get_track(&track_id);
        if let Some(e) = &existing
            && e.is_ready()
            && e.file.as_deref() != Some(file.as_str())
        {
            bump(&mut stats, "duplicate", 1);
            tracing::info!("{file} duplicates {}; left as it is", e.file.as_deref().unwrap_or(""));
            continue;
        }
        let meta = match deezer.full_track_meta(&key).await {
            Ok(meta) => meta,
            Err(e) => {
                bump(&mut stats, "not_in_catalog", 1);
                tracing::info!("{file}: {e}");
                continue;
            }
        };
        let info = match tools.probe(&path).await {
            Ok(info) => info,
            Err(e) => {
                bump(&mut stats, "unreadable", 1);
                tracing::warn!("cannot read {file}: {e}");
                continue;
            }
        };
        if let Some(d) = as_f64(meta.get("duration")).filter(|d| *d != 0.0)
            && (info.duration - d).abs() > tolerance
        {
            bump(&mut stats, "other_version", 1);
            tracing::info!("{file} is {:.0}s, catalog says {d}s; left as it is", info.duration);
            continue;
        }
        bump(&mut stats, "adopted", 1);
        if dry_run {
            continue;
        }
        let _ = std::fs::create_dir_all(work_dir);
        let cover = cover_for_tags(covers, tools, &as_str(meta.get("cover")), work_dir).await;
        let (p, m) = (path.clone(), meta.clone());
        if let Ok(Err(e)) = tokio::task::spawn_blocking(move || crate::tagging::write_tags(&p, &m, cover.as_deref())).await {
            tracing::warn!("cannot tag {file}: {e}");
        }
        let bogus = store.get_track(&format!("yt:{key}-yt")); // misread by an earlier migrate-legacy
        let local_id = existing.and_then(|e| e.local_id).or_else(|| bogus.as_ref().and_then(|b| b.local_id.clone()));
        store.remember_track(&track_id, &meta);
        store.update_track(
            &track_id,
            &[
                Field::status("ready"),
                Field::File(Some(file.clone())),
                Field::Audio(Some(info.as_json())),
                Field::Source(Some("legacy-youtube".into())),
                Field::Error(None),
                Field::LocalId(local_id),
            ],
        );
        if let Some(b) = bogus {
            store.update_track(&b.id, &[Field::status("new"), Field::File(None), Field::LocalId(None)]);
            identity.link(&b.id, &track_id, "legacy");
        }
        if let Some(t) = store.get_track(&track_id) {
            identity.register(&t);
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{have, make, need};
    use crate::testutil::{FakeDeezer, dz_track, m, serve};
    use crate::util::Json;

    struct Legacy {
        _dir: tempfile::TempDir,
        store: Arc<Store>,
        music: PathBuf,
        state: PathBuf,
        tools: Tools,
    }

    fn legacy() -> Legacy {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        let ready = make(&music.join("deezer-111.m4a"), 25, "aac", true);
        crate::tagging::write_tags(&ready, &m(json!({"title": "Old Song", "artist": "TVETH / Bato", "album": "RAW", "album_artist": "TVETH", "track": 2})), None).unwrap();
        std::fs::write(music.join("deezer-111.lrc"), "[00:01]la").unwrap();
        make(&music.join("Persona 3 - Aria.m4a"), 25, "aac", false); // not ours: ignored
        let state = json!({"jobs": {
            "deezer:111": {"id": "deezer:111", "title": "Old Song", "artist": "TVETH / Bato", "status": "ready",
                           "local_id": "NAV111", "file": "/music/deezer-111.m4a", "album_id": "900"},
            "deezer:222": {"id": "deezer:222", "title": "Never Came", "artist": "X", "status": "failed",
                           "pending_star_users": {"carol": 1.0}, "storage_reasons": ["star:carol"]},
            "deezer:333": {"id": "deezer:333", "title": "Escape", "artist": "X", "status": "ready",
                           "file": "/music/../../etc/hostname"},
            "fallback:abc": {"id": "fallback:abc", "kind": "fallback", "title": "Mania", "artist": "SchuberTEKK",
                             "status": "failed", "pending_star_users": {"carol": 2.0}},
            "deezer-album:900": {"id": "deezer-album:900", "status": "idle"},
        }, "imports": {}});
        let state_file = dir.path().join("state.json");
        std::fs::write(&state_file, state.to_string()).unwrap();
        Legacy { store: Arc::new(Store::memory()), music, state: state_file, tools: Tools::default(), _dir: dir }
    }

    fn n(stats: &Json, key: &str) -> i64 {
        stats.get(key).and_then(Value::as_i64).unwrap_or(0)
    }

    #[tokio::test]
    async fn migrate_adopts_files_state_and_likes() {
        need!("ffmpeg");
        let l = legacy();
        let stats = migrate_legacy(&l.store, &l.tools, &l.music, Some(&l.state), "/music").await;
        let track = l.store.get_track("deezer:111").unwrap();
        assert!(track.is_ready());
        assert_eq!((track.local_id.as_deref(), track.album_id.as_deref()), (Some("NAV111"), Some("900")));
        assert_eq!(track.artists(), ["TVETH", "Bato"]);
        assert!((as_f64(track.audio.get("duration")).unwrap() - 25.0).abs() < 1.0);
        let stars: Vec<String> = l.store.pending_stars(Some("carol")).into_iter().map(|(_, t, _)| t).collect();
        assert_eq!(stars, ["deezer:222"]);
        assert_eq!(l.store.get_job("deezer:222", "download").unwrap().reason.as_deref(), Some("like:carol"));
        let imp = l.store.get_import("legacy-carol").unwrap();
        assert_eq!(imp["rows"][0]["title"], "Mania");
        assert_eq!(n(&stats, "files_adopted"), 1);
        // B3: a path escaping the music directory is not adopted.
        assert!(!l.store.get_track("deezer:333").unwrap().is_ready());
        let again = migrate_legacy(&l.store, &l.tools, &l.music, Some(&l.state), "/music").await;
        assert_eq!(n(&again, "files_known"), 1, "idempotent");
        assert!(!again.contains_key("legacy_import_rows"));
    }

    #[tokio::test]
    async fn relayout_moves_file_and_sidecar() {
        need!("ffmpeg");
        let l = legacy();
        migrate_legacy(&l.store, &l.tools, &l.music, Some(&l.state), "/music").await;
        assert_eq!(n(&relayout(&l.store, &l.music, true), "moved"), 1);
        assert!(l.music.join("deezer-111.m4a").exists());
        let stats = relayout(&l.store, &l.music, false);
        let target = l.music.join("TVETH/RAW/1-02 - Old Song [deezer-111].m4a");
        assert_eq!(n(&stats, "moved"), 1);
        assert!(target.is_file() && target.with_extension("lrc").is_file());
        assert!(!l.music.join("deezer-111.m4a").exists());
        assert_eq!(l.store.get_track("deezer:111").unwrap().file.as_deref(), Some(target.to_str().unwrap()));
        assert_eq!(n(&relayout(&l.store, &l.music, false), "kept"), 1);
    }

    #[test]
    fn relayout_never_overwrites_and_finishes_an_interrupted_move() {
        let dir = tempfile::tempdir().unwrap();
        let music = dir.path().join("music");
        let store = Store::memory();
        let target = music.join("A/B/Song [deezer-1].m4a");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let current = music.join("deezer-1.m4a");
        std::fs::write(&current, b"ours").unwrap();
        std::fs::write(&target, b"someone else's").unwrap();
        store.remember_track("deezer:1", &m(json!({"title": "Song", "artist": "A", "album": "B"})));
        store.update_track("deezer:1", &[Field::status("ready"), Field::File(Some(current.display().to_string()))]);
        let stats = relayout(&store, &music, false);
        assert_eq!((n(&stats, "moved"), n(&stats, "skipped")), (0, 1));
        assert_eq!(std::fs::read(&target).unwrap(), b"someone else's", "B5: target kept");
        // A crash after the link: both names are the same file; the next run completes it.
        std::fs::remove_file(&target).unwrap();
        std::fs::hard_link(&current, &target).unwrap();
        assert_eq!(n(&relayout(&store, &music, false), "moved"), 1);
        assert!(!current.exists() && std::fs::read(&target).unwrap() == b"ours");
        // Files outside the music directory are never moved.
        let outside = dir.path().join("deezer-2.m4a");
        std::fs::write(&outside, b"x").unwrap();
        store.remember_track("deezer:2", &m(json!({"title": "Two", "artist": "A", "album": "B"})));
        store.update_track("deezer:2", &[Field::status("ready"), Field::File(Some(outside.display().to_string()))]);
        assert_eq!(n(&relayout(&store, &music, false), "skipped"), 1);
        assert!(outside.exists());
    }

    #[tokio::test]
    async fn remux_dash_rewrites_and_cleans_temp_files() {
        need!("ffmpeg");
        let l = legacy();
        assert_eq!(n(&remux_dash(&l.tools, &l.music, true).await, "remuxed"), 1);
        let stale = make(&l.music.join(".deezer-111.remux.m4a"), 5, "aac", false); // left behind by an interrupted run
        let stats = remux_dash(&l.tools, &l.music, false).await;
        assert_eq!(Value::Object(stats), json!({"checked": 2, "remuxed": 1, "truncated": 0, "failed": 0}));
        assert!(!stale.exists(), "B8");
        assert_ne!(l.tools.probe(&l.music.join("deezer-111.m4a")).await.unwrap().brand, "dash");
        assert_eq!(crate::tagging::read_tags(&l.music.join("deezer-111.m4a"))["title"], ["Old Song"], "tags survive");
    }

    #[tokio::test]
    async fn repair_covers_reembeds_truncated_artwork() {
        need!("ffmpeg");
        let mut full = b"\xff\xd8\xff\xe0".to_vec();
        full.extend(vec![b'x'; 5000]);
        full.extend(b"\xff\xd9");
        let body = full.clone();
        let server = serve(axum::Router::new().route(
            "/c.jpg",
            axum::routing::get(move || {
                let b = body.clone();
                async move { ([("content-type", "image/jpeg")], b) }
            }),
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let covers = Covers::new(&dir.path().join("covers"));
        let music = dir.path().join("music");
        let folder = music.join("TVETH/RAW");
        std::fs::create_dir_all(&folder).unwrap();
        let song = make(&folder.join("1-02 - Old Song [deezer-111].m4a"), 5, "aac", false);
        crate::tagging::write_tags(&song, &m(json!({"title": "Old Song", "artist": "TVETH"})), Some(&full[..1378])).unwrap();
        let fine = make(&folder.join("1-03 - Other [deezer-112].m4a"), 5, "aac", false);
        crate::tagging::write_tags(&fine, &m(json!({"title": "Other", "artist": "TVETH"})), Some(&full)).unwrap();
        let store = Store::memory();
        store.remember_track("deezer:111", &m(json!({"title": "Old Song", "cover": format!("{}/c.jpg", server.url)})));
        let tools = Tools::default();
        let work = dir.path().join("work");
        let dry = repair_covers(&store, &tools, &music, &covers, &work, true).await;
        assert_eq!(Value::Object(dry), json!({"checked": 2, "broken": 1, "repaired": 1, "unresolved": 0}));
        assert_eq!(crate::tagging::embedded_cover(&song).unwrap(), &full[..1378]);
        repair_covers(&store, &tools, &music, &covers, &work, false).await;
        assert_eq!(crate::tagging::embedded_cover(&song).unwrap(), full);
        assert_eq!(crate::tagging::read_tags(&song)["title"], ["Old Song"]);
        assert_eq!(n(&repair_covers(&store, &tools, &music, &covers, &work, false).await, "broken"), 0);
    }

    #[tokio::test]
    async fn adopt_youtube_fallbacks_tags_files_from_the_catalog() {
        need!("ffmpeg");
        let fake = FakeDeezer::empty();
        let track = |id, title, position, duration| {
            let mut t = dz_track(id, title, "тёмный принц", 100, "RAW", position, &[]);
            t.insert("duration".into(), json!(duration));
            t
        };
        fake.add_album(100, "RAW", "TVETH", 1, vec![track(4145181542, "BURN", 2, 20), track(4145181543, "Long", 3, 200)]);
        let server = serve(fake.router()).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::memory());
        let deezer = Deezer::new(store.clone(), 100.0, &server.url, 60.0);
        let covers = Covers::new(&dir.path().join("covers"));
        let music = dir.path().join("music");
        std::fs::create_dir_all(&music).unwrap();
        let burn = make(&music.join("yt-4145181542-yt.m4a"), 20, "aac", false);
        crate::tagging::write_tags(&burn, &m(json!({"title": "BURN", "artist": "тёмный принц"})), None).unwrap();
        let other = make(&music.join("yt-4145181543-yt.m4a"), 20, "aac", false); // 20 s vs 200 s: another version
        store.remember_track("yt:4145181542-yt", &m(json!({"title": "BURN"}))); // misread by the old regex
        store.update_track("yt:4145181542-yt", &[Field::status("ready"), Field::File(Some(burn.display().to_string())), Field::LocalId(Some("ND1".into()))]);
        let (tools, work) = (Tools::default(), dir.path().join("work"));
        let dry = adopt_youtube_fallbacks(&store, &tools, &music, &deezer, &covers, &work, true, 15.0).await;
        assert_eq!(Value::Object(dry), json!({"found": 2, "adopted": 1, "other_version": 1}));
        assert!(!crate::tagging::read_tags(&burn).contains_key("album"));
        adopt_youtube_fallbacks(&store, &tools, &music, &deezer, &covers, &work, false, 15.0).await;
        let tags = crate::tagging::read_tags(&burn);
        assert_eq!(tags["album"], ["RAW"]);
        assert!(tags["tracknumber"][0].starts_with('2'));
        let t = store.get_track("deezer:4145181542").unwrap();
        assert_eq!((t.status.as_str(), t.file.as_deref(), t.local_id.as_deref()), ("ready", burn.to_str(), Some("ND1")));
        assert_eq!(store.get_track("yt:4145181542-yt").unwrap().status, "new");
        assert!(!crate::tagging::read_tags(&other).contains_key("album"));
        // relayout now names it like any other catalog track
        assert_eq!(n(&relayout(&store, &music, false), "moved"), 1);
        assert!(music.join("TVETH/RAW/1-02 - BURN [deezer-4145181542].m4a").is_file());
    }
}

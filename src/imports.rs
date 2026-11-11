//! Bulk import of plain-text track lists ("Artist - Title" per line).
//!
//! Each line is matched against the catalog with `matching::score_catalog_candidate`
//! (title +40, all credited artists +35, album +15, duration +10, ISRC = certain,
//! version markers penalized). Only candidates crediting at least one of the
//! requested artists count: a title alone ("No Lie", "Awake") names too many songs.
//! A confident match is used; otherwise the best credited candidate is still taken
//! but marked `fallback` in the report. Lines Deezer does not know are looked up
//! on YouTube Music, then SoundCloud (whose matching checks the artist too).
//!
//! Matched tracks are liked for the importing user and downloaded in the background
//! lane (they never slow down interactive playback). With `playlist` the import
//! also maintains a Navidrome playlist in the original list order.

use crate::catalog::Catalog;
use crate::db::Store;
use crate::deezer::{self, Deezer, DeezerError};
use crate::identity::Identity;
use crate::ids;
use crate::matching::{self, Credits, Wanted};
use crate::navidrome::Navidrome;
use crate::pipeline::Fetcher;
use crate::scheduler::{BACKGROUND, Scheduler};
use crate::util::{Json, as_f64, as_str, opt_str, truthy};
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub const MAX_LINES: usize = 5000;
pub const MAX_LINE: usize = 500;
pub const CONFIDENT: f64 = 75.0;
pub const MARGIN: f64 = 5.0;

static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\d+\s*[.)\-:]?\s+").unwrap());
static DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[]\s*(\d{1,2}):(\d{2})\s*[\)\]]\s*$").unwrap());
static ALBUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\[([^\]]+)\]\s*$").unwrap());
static SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+[-–—]\s+").unwrap());

/// `{"artist", "title", "album", "duration"}` of one line, or None if it is not "Artist - Title".
pub fn parse_line(raw: &str) -> Option<Json> {
    let mut text = raw.trim().trim_start_matches('\u{feff}').to_string();
    if text.is_empty() {
        return None;
    }
    text = NUMBER.replace(&text, "").into_owned();
    let mut duration = Value::Null;
    if let Some(c) = DURATION.captures(&text) {
        let (m, s): (i64, i64) = (c[1].parse().ok()?, c[2].parse().ok()?);
        duration = json!(m * 60 + s);
        text.truncate(c.get(0)?.start());
    }
    let mut album = Value::Null;
    if let Some(c) = ALBUM.captures(&text) {
        album = json!(c[1].trim());
        text.truncate(c.get(0)?.start());
    }
    let mut parts = SEPARATOR.splitn(&text, 2);
    let (artist, title) = (parts.next()?.trim(), parts.next()?.trim());
    if artist.is_empty() || title.is_empty() {
        return None;
    }
    let v = json!({"artist": artist, "title": title, "album": album, "duration": duration});
    v.as_object().cloned()
}

pub fn parse_text(text: &str) -> Result<Vec<Json>, String> {
    if text.lines().count() > MAX_LINES {
        return Err(format!("at most {MAX_LINES} lines"));
    }
    let mut rows = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let mut row = Json::new();
        row.insert("line".into(), json!(i + 1));
        let parsed = (raw.chars().count() <= MAX_LINE)
            .then(|| parse_line(raw))
            .flatten();
        row.insert(
            "input".into(),
            json!(raw.chars().take(MAX_LINE).collect::<String>()),
        );
        match parsed {
            Some(p) => {
                row.insert("status".into(), json!("pending"));
                row.extend(p);
            }
            None => {
                row.insert("status".into(), json!("invalid"));
            }
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Err("no lines to import".into());
    }
    Ok(rows)
}

fn rows_of(imp: &Json) -> Vec<Json> {
    imp.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_object().cloned())
        .collect()
}

fn rows_value(rows: &[Json]) -> Value {
    Value::Array(rows.iter().cloned().map(Value::Object).collect())
}

fn save(store: &Store, imp: &Json, status: &str, options: &Json, rows: &[Json]) {
    let (id, user) = (as_str(imp.get("id")), as_str(imp.get("username")));
    store.save_import(
        &id,
        &user,
        status,
        &Value::Object(options.clone()),
        &rows_value(rows),
    );
}

/// The progress report of an import (rows annotated with their download state).
pub fn report(store: &Store, identity: &Identity, import_id: &str) -> Option<Json> {
    let imp = store.get_import(import_id)?;
    let mut rows = rows_of(&imp);
    let canonical: Vec<String> = rows
        .iter()
        .filter_map(|r| opt_str(r.get("track_id")))
        .map(|t| identity.canonical(&t))
        .collect();
    let tracks = store.get_tracks(canonical.iter().map(String::as_str));
    let (mut counts, mut downloads) = (Json::new(), Json::new());
    let bump = |m: &mut Json, k: String| {
        let n = m.get(&k).and_then(Value::as_i64).unwrap_or(0);
        m.insert(k, json!(n + 1));
    };
    for row in &mut rows {
        bump(&mut counts, as_str(row.get("status")));
        if let Some(tid) = opt_str(row.get("track_id")) {
            let track = tracks.get(&identity.canonical(&tid));
            let state = track
                .map(|t| t.status.clone())
                .unwrap_or_else(|| "unknown".into());
            row.insert("download".into(), json!(state));
            if let Some(t) = track
                && t.status == "failed"
                && let Some(e) = t.error.as_deref().filter(|e| !e.is_empty())
            {
                row.insert("download_error".into(), json!(crate::util::tail(e, 200)));
            }
            bump(&mut downloads, state);
        }
    }
    let done = rows
        .iter()
        .filter(|r| as_str(r.get("status")) != "pending")
        .count();
    let mut options = crate::util::obj(imp.get("options"));
    let playlist_id = options.remove("playlist_id").unwrap_or(Value::Null);
    let v = json!({
        "id": imp["id"], "status": imp["status"], "created_at": imp["created_at"], "options": options,
        "playlist_id": playlist_id, "progress": {"done": done, "total": rows.len()},
        "counts": counts, "downloads": downloads, "rows": rows_value(&rows),
    });
    v.as_object().cloned()
}

/// Whether the import matched a line to a song that is not the requested one.
fn wrong_match(row: &Json, m: &Json) -> bool {
    let artists = matching::split_artists(&as_str(row.get("artist")));
    let title = as_str(row.get("title"));
    match as_str(row.get("status")).as_str() {
        "matched" | "fallback" => {
            matching::artists_match(&artists, Credits::Text(&as_str(m.get("artist")))) == 0.0
                || !matching::title_matches(&title, &as_str(m.get("title")), 0.8)
        }
        "matched_youtube" | "matched_soundcloud" => {
            let wanted = Wanted {
                title,
                artists,
                ..Default::default()
            };
            matching::score_source_candidate(
                &wanted,
                &as_str(m.get("title")),
                &as_str(m.get("artist")),
                None,
                false,
                &[],
                10.0,
            )
            .rejected
            .is_some()
        }
        _ => false,
    }
}

/// Match again the rows whose catalog match credits none of the requested artists.
///
/// Earlier versions accepted such title-only matches as `fallback`. The like the
/// import set on the wrong song is taken back (as that user, on their next
/// request) and the line goes through the current matcher again.
pub fn recheck(store: &Store, identity: &Identity, import_id: &str, dry_run: bool) -> Option<Json> {
    let imp = store.get_import(import_id)?;
    let username = as_str(imp.get("username"));
    let mut rows = rows_of(&imp);
    let (mut wrong, mut unstar, mut lines) = (0, 0, Vec::new());
    for row in &mut rows {
        let m = crate::util::obj(row.get("match"));
        let Some(track_id) = opt_str(row.get("track_id")) else {
            continue;
        };
        if !wrong_match(row, &m) {
            continue;
        }
        wrong += 1;
        let line = format!(
            "{}  ->  {} - {}",
            as_str(row.get("input")),
            as_str(m.get("artist")),
            as_str(m.get("title"))
        );
        lines.push(line);
        if dry_run {
            continue;
        }
        if !store.remove_pending_star(&username, &track_id)
            && let Some(local_id) = store
                .get_track(&identity.canonical(&track_id))
                .and_then(|t| t.local_id)
        {
            store.add_pending_unstar(&username, &local_id);
            unstar += 1;
        }
        tracing::info!(
            "import {import_id} line {}: {:?} was matched to {:?}; matching again",
            as_str(row.get("line")),
            as_str(row.get("input")),
            lines.last()
        );
        for key in [
            "track_id",
            "match",
            "alternatives",
            "download",
            "download_error",
        ] {
            row.remove(key);
        }
        row.insert("status".into(), json!("pending"));
    }
    if wrong > 0 && !dry_run {
        save(
            store,
            &imp,
            "queued",
            &crate::util::obj(imp.get("options")),
            &rows,
        );
    }
    json!({"wrong": wrong, "unstar": unstar, "lines": lines})
        .as_object()
        .cloned()
}

pub struct Importer {
    store: Arc<Store>,
    identity: Identity,
    catalog: Arc<Catalog>,
    deezer: Arc<Deezer>,
    finder: Arc<dyn Fetcher>,
    scheduler: Arc<Scheduler>,
    navidrome: Arc<Navidrome>,
    queue: mpsc::UnboundedSender<String>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<String>>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    pub playlist_interval: Duration,
}

impl Importer {
    pub fn new(
        store: Arc<Store>,
        identity: Identity,
        catalog: Arc<Catalog>,
        deezer: Arc<Deezer>,
        finder: Arc<dyn Fetcher>,
        scheduler: Arc<Scheduler>,
        navidrome: Arc<Navidrome>,
    ) -> Self {
        let (queue, receiver) = mpsc::unbounded_channel();
        Self {
            store,
            identity,
            catalog,
            deezer,
            finder,
            scheduler,
            navidrome,
            queue,
            receiver: Mutex::new(Some(receiver)),
            tasks: Mutex::new(Vec::new()),
            playlist_interval: Duration::from_secs(30),
        }
    }

    pub fn start(self: &Arc<Self>) {
        for imp in self.store.unfinished_imports() {
            self.enqueue(&as_str(imp.get("id")));
        }
        let Some(mut rx) = self.receiver.lock().take() else {
            return;
        };
        let me = self.clone();
        let worker = tokio::spawn(async move {
            while let Some(id) = rx.recv().await {
                let run = tokio::spawn(deezer::BACKGROUND.scope(true, {
                    let me = me.clone();
                    let id = id.clone();
                    async move { me.run(&id).await }
                }));
                if let Err(e) = run.await {
                    tracing::error!("import {id} crashed: {e}");
                    if let Some(imp) = me.store.get_import(&id) {
                        save(
                            &me.store,
                            &imp,
                            "failed",
                            &crate::util::obj(imp.get("options")),
                            &rows_of(&imp),
                        );
                    }
                }
            }
        });
        let me = self.clone();
        let playlists = tokio::spawn(async move {
            loop {
                tokio::time::sleep(me.playlist_interval).await;
                me.sync_playlists().await;
            }
        });
        self.tasks.lock().extend([worker, playlists]);
    }

    pub async fn stop(&self) {
        let tasks: Vec<_> = self.tasks.lock().drain(..).collect();
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    }

    pub fn enqueue(&self, import_id: &str) {
        let _ = self.queue.send(import_id.to_string());
    }

    // -------------------------------------------------------------------- API
    pub fn create(
        &self,
        username: &str,
        text: &str,
        like: bool,
        playlist: Option<&str>,
    ) -> Result<Json, String> {
        let rows = parse_text(text)?;
        let import_id = format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff);
        let playlist = playlist.map(str::trim).filter(|p| !p.is_empty());
        let options = json!({"like": like, "playlist": playlist, "playlist_id": null});
        self.store
            .save_import(&import_id, username, "queued", &options, &rows_value(&rows));
        self.enqueue(&import_id);
        Ok(self.report(&import_id).unwrap_or_default())
    }

    pub fn report(&self, import_id: &str) -> Option<Json> {
        report(&self.store, &self.identity, import_id)
    }

    pub fn recheck(&self, import_id: &str, dry_run: bool) -> Option<Json> {
        recheck(&self.store, &self.identity, import_id, dry_run)
    }

    // ----------------------------------------------------------------- worker
    pub async fn run(&self, import_id: &str) {
        let Some(imp) = self.store.get_import(import_id) else {
            return;
        };
        if matches!(as_str(imp.get("status")).as_str(), "completed" | "failed") {
            return;
        }
        let username = as_str(imp.get("username"));
        let options = crate::util::obj(imp.get("options"));
        let mut rows = rows_of(&imp);
        save(&self.store, &imp, "processing", &options, &rows);
        let mut last_save = Instant::now();
        for i in 0..rows.len() {
            if as_str(rows[i].get("status")) != "pending" {
                continue;
            }
            if let Err(e) = self.match_row(&mut rows[i]).await {
                rows[i].insert("status".into(), json!("error"));
                rows[i].insert("error".into(), json!(e.to_string()));
            }
            if let Some(track_id) = opt_str(rows[i].get("track_id")) {
                if truthy(options.get("like")) {
                    self.store.add_pending_star(&username, &track_id);
                }
                if let Some(track) = self.store.get_track(&self.identity.canonical(&track_id))
                    && !track.is_ready()
                {
                    self.scheduler.download(
                        &track.id,
                        BACKGROUND,
                        &format!("like-import:{username}"),
                        track.status == "failed",
                    );
                }
            }
            if last_save.elapsed() > Duration::from_secs(2) {
                save(&self.store, &imp, "processing", &options, &rows);
                last_save = Instant::now();
            }
        }
        save(&self.store, &imp, "completed", &options, &rows);
        let counts = self
            .report(import_id)
            .and_then(|r| r.get("counts").cloned())
            .unwrap_or_default();
        tracing::info!("import {import_id} of {username} finished: {counts}");
    }

    async fn match_row(&self, row: &mut Json) -> Result<(), DeezerError> {
        let (artist, title) = (as_str(row.get("artist")), as_str(row.get("title")));
        let wanted = Wanted {
            title: title.clone(),
            artists: matching::split_artists(&artist),
            album: opt_str(row.get("album")),
            duration: as_f64(row.get("duration")),
            ..Default::default()
        };
        let mut found = self
            .deezer
            .search_tracks(&format!("{artist} {title}"), 15, 0)
            .await?;
        if found.is_empty() {
            found = self.deezer.search_tracks(&title, 15, 0).await?;
        }
        let mut best = if found.is_empty() {
            Vec::new()
        } else {
            self.rank(&wanted, &found).await
        };
        best.retain(|c| {
            as_f64(c.get("artist_share")).unwrap_or(0.0) > 0.0
                && matching::title_matches(&title, &as_str(c.get("title")), 0.8)
        });
        let score = |c: &Json| as_f64(c.get("score")).unwrap_or(0.0);
        let set_other = |row: &mut Json, other: Json| {
            row.insert(
                "status".into(),
                json!(format!("matched_{}", as_str(other.get("source")))),
            );
            row.insert("track_id".into(), other["id"].clone());
            row.insert("match".into(), Value::Object(other));
        };
        if best.first().is_some_and(|top| score(top) < 40.0) {
            // Nothing even close in Deezer: another catalog may have the real track.
            if let Some(other) = self.other_source(&wanted).await {
                set_other(row, other);
                return Ok(());
            }
        }
        if let Some(top) = best.first() {
            let confident = score(top) >= CONFIDENT
                && (best.len() < 2
                    || score(top) - score(&best[1]) >= MARGIN
                    || best[1].get("id") == top.get("id"));
            let pick = |c: &Json, keys: &[&str]| -> Value {
                Value::Object(
                    keys.iter()
                        .map(|k| (k.to_string(), c.get(*k).cloned().unwrap_or(Value::Null)))
                        .collect(),
                )
            };
            row.insert(
                "status".into(),
                json!(if confident { "matched" } else { "fallback" }),
            );
            row.insert("track_id".into(), top["id"].clone());
            row.insert(
                "match".into(),
                pick(
                    top,
                    &[
                        "id", "title", "artist", "album", "duration", "score", "reasons",
                    ],
                ),
            );
            let alternatives: Vec<Value> = best
                .iter()
                .skip(1)
                .take(3)
                .map(|c| pick(c, &["id", "title", "artist", "album", "duration", "score"]))
                .collect();
            row.insert("alternatives".into(), Value::Array(alternatives));
            return Ok(());
        }
        match self.other_source(&wanted).await {
            Some(other) => set_other(row, other),
            None => {
                row.insert("status".into(), json!("not_found"));
            }
        }
        Ok(())
    }

    /// Score candidates; the top few are re-scored with full credits (collaborations).
    async fn rank(&self, wanted: &Wanted, found: &[Json]) -> Vec<Json> {
        self.catalog.remember_deezer(found, None);
        let mut first: Vec<(f64, &Json)> = found
            .iter()
            .map(|r| {
                (
                    matching::score_catalog_candidate(wanted, &deezer::track_meta(r, None)).score,
                    r,
                )
            })
            .collect();
        first.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut ranked = Vec::new();
        for (_, r) in first.into_iter().take(5) {
            let id = as_str(r.get("id"));
            let meta = match self.deezer.full_track_meta(&id).await {
                Ok(meta) => {
                    self.store.remember_track(&ids::track(&id), &meta);
                    meta
                }
                Err(_) => deezer::track_meta(r, None),
            };
            let scored = matching::score_catalog_candidate(wanted, &meta);
            let list: Vec<String> = meta
                .get("artists")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|v| as_str(Some(v)))
                .collect();
            let artist = as_str(meta.get("artist"));
            let share = if list.is_empty() {
                matching::artists_match(&wanted.artists, Credits::Text(&artist))
            } else {
                matching::artists_match(&wanted.artists, Credits::List(&list))
            };
            let v = json!({
                "id": ids::track(&id), "title": meta.get("title"), "artist": meta.get("artist"),
                "album": meta.get("album"), "duration": meta.get("duration"), "score": scored.score,
                "reasons": scored.reasons, "rank": meta.get("rank").cloned().unwrap_or(json!(0)), "artist_share": share,
            });
            ranked.extend(v.as_object().cloned());
        }
        let key = |c: &Json, k: &str| as_f64(c.get(k)).unwrap_or(0.0);
        ranked.sort_by(|a, b| {
            key(b, "score")
                .total_cmp(&key(a, "score"))
                .then(key(b, "rank").total_cmp(&key(a, "rank")))
        });
        ranked
    }

    /// The best YouTube/SoundCloud match, remembered as a track.
    async fn other_source(&self, wanted: &Wanted) -> Option<Json> {
        let found = self.finder.find(wanted, &HashSet::new()).await;
        let cand = found.into_iter().next()?;
        let track_id = if cand.source == "youtube" {
            ids::youtube(&cand.reference)
        } else if !cand.reference.is_empty() && cand.reference.chars().all(|c| c.is_ascii_digit()) {
            ids::soundcloud(&cand.reference)
        } else {
            return None;
        };
        let meta = json!({
            "source": cand.source, "title": wanted.title, "artists": wanted.artists,
            "artist": wanted.artists.join(", "), "duration": cand.duration, "album": cand.album,
            "cover": cand.thumbnail, "url": cand.url,
        });
        self.store.remember_track(&track_id, meta.as_object()?);
        json!({
            "id": track_id, "source": cand.source, "title": cand.title, "artist": cand.uploader,
            "duration": cand.duration, "score": cand.score,
        })
        .as_object()
        .cloned()
    }

    // -------------------------------------------------------------- playlists
    /// Keep each import playlist equal to its downloaded tracks, in list order.
    pub async fn sync_playlists(&self) {
        for import_id in self.store.imports_with_playlist() {
            let Some(imp) = self.store.get_import(&import_id) else {
                continue;
            };
            let username = as_str(imp.get("username"));
            let Some(creds) = self.navidrome.credentials_for(&username) else {
                continue;
            };
            let mut local_ids: Vec<String> = Vec::new();
            for row in rows_of(&imp) {
                let Some(tid) = opt_str(row.get("track_id")) else {
                    continue;
                };
                if let Some(track) = self.store.get_track(&self.identity.canonical(&tid))
                    && track.is_ready()
                    && let Some(local) = track.local_id.filter(|l| !l.is_empty())
                    && !local_ids.contains(&local)
                {
                    local_ids.push(local);
                }
            }
            let mut options = crate::util::obj(imp.get("options"));
            let playlist_id = opt_str(options.get("playlist_id")).filter(|p| !p.is_empty());
            if as_f64(options.get("synced")) == Some(local_ids.len() as f64)
                && playlist_id.is_some()
            {
                continue;
            }
            let name = as_str(options.get("playlist"));
            let extra = match &playlist_id {
                Some(id) => [("playlistId", id.as_str())],
                None => [("name", name.as_str())],
            };
            let root = match self
                .navidrome
                .call_many(
                    "createPlaylist",
                    &creds,
                    &[("songId", local_ids.clone())],
                    &extra,
                )
                .await
            {
                Ok(root) => root,
                Err(e) => {
                    tracing::warn!("import playlist {name} not updated: {e}");
                    continue;
                }
            };
            let id = playlist_id.unwrap_or_else(|| {
                as_str(crate::util::path(&Value::Object(root), &["playlist", "id"]))
            });
            options.insert("playlist_id".into(), json!(id));
            options.insert("synced".into(), json!(local_ids.len()));
            save(
                &self.store,
                &imp,
                &as_str(imp.get("status")),
                &options,
                &rows_of(&imp),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Field;
    use crate::pipeline::tests::{FakeFetcher, cand};
    use crate::subsonic::params::Params;
    use crate::testutil::{FakeDeezer, FakeNavidrome, Server, dz_track, m, serve};

    #[test]
    fn parse_line_variants() {
        let cases = [
            (
                "1. TVETH, BATO - Hella Quality",
                json!({"artist": "TVETH, BATO", "title": "Hella Quality", "album": null, "duration": null}),
            ),
            (
                "12) Юпи – Порезы на руках",
                json!({"artist": "Юпи", "title": "Порезы на руках", "album": null, "duration": null}),
            ),
            (
                "Imagine Dragons - Radioactive [Night Visions] (3:07)",
                json!({"artist": "Imagine Dragons", "title": "Radioactive", "album": "Night Visions", "duration": 187}),
            ),
            (
                "Jay-Z - 99 Problems",
                json!({"artist": "Jay-Z", "title": "99 Problems", "album": null, "duration": null}),
            ),
            (
                "A - B - C",
                json!({"artist": "A", "title": "B - C", "album": null, "duration": null}),
            ),
            (
                "\u{feff}A - B",
                json!({"artist": "A", "title": "B", "album": null, "duration": null}),
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(Value::Object(parse_line(line).unwrap()), expected, "{line}");
        }
        assert_eq!(parse_line("just words"), None);
        assert_eq!(parse_line(" - B"), None);
    }

    #[test]
    fn parse_text_marks_invalid_and_skips_blank() {
        let rows = parse_text("A - B\n\njust words\n").unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|r| (r["line"].clone(), as_str(r.get("status"))))
            .collect();
        assert_eq!(
            got,
            [
                (json!(1), "pending".to_string()),
                (json!(3), "invalid".to_string())
            ]
        );
        assert!(parse_text("\n \n").is_err());
        assert!(parse_text(&"A - B\n".repeat(MAX_LINES + 1)).is_err());
        let long = parse_text(&format!("A - {}", "x".repeat(MAX_LINE))).unwrap();
        assert_eq!(
            (
                as_str(long[0].get("status")).as_str(),
                as_str(long[0].get("input")).chars().count()
            ),
            ("invalid", MAX_LINE)
        );
    }

    struct World {
        importer: Arc<Importer>,
        store: Arc<Store>,
        deezer: FakeDeezer,
        navidrome: FakeNavidrome,
        finder: Arc<FakeFetcher>,
        nav: Arc<Navidrome>,
        _servers: (Server, Server),
    }

    async fn world() -> World {
        let deezer = FakeDeezer::default();
        let navidrome = FakeNavidrome::default();
        let servers = (
            serve(deezer.router()).await,
            serve(navidrome.router()).await,
        );
        let store = Arc::new(Store::memory());
        let identity = Identity::new(store.clone());
        let nav = Arc::new(Navidrome::new(&servers.1.url, 60.0));
        let catalog = Arc::new(Catalog::new(store.clone(), identity.clone(), nav.clone()));
        let dz = Arc::new(Deezer::new(store.clone(), 100.0, &servers.0.url, 60.0));
        let finder = Arc::new(FakeFetcher::default());
        let scheduler = Arc::new(Scheduler::new(store.clone(), 1, 4));
        let importer = Arc::new(Importer::new(
            store.clone(),
            identity,
            catalog,
            dz,
            finder.clone(),
            scheduler,
            nav.clone(),
        ));
        World {
            importer,
            store,
            deezer,
            navidrome,
            finder,
            nav,
            _servers: servers,
        }
    }

    fn with(
        c: crate::sources::Candidate,
        title: &str,
        uploader: &str,
    ) -> crate::sources::Candidate {
        crate::sources::Candidate {
            title: title.into(),
            uploader: format!("{uploader} - Topic"),
            official: true,
            ..c
        }
    }

    fn rows(report: &Json) -> Vec<Json> {
        rows_of(report)
    }

    #[tokio::test]
    async fn import_matches_collabs_editions_and_other_sources() {
        let w = world().await;
        let mut radio = dz_track(
            63510358,
            "Radioactive",
            "Imagine Dragons",
            200,
            "Night Visions",
            1,
            &[],
        );
        radio.insert("duration".into(), json!(187));
        let mut single = dz_track(
            74506420,
            "Radioactive",
            "Imagine Dragons",
            201,
            "Radioactive",
            1,
            &[],
        );
        single.insert("duration".into(), json!(276));
        w.deezer
            .add_album(200, "Night Visions", "Imagine Dragons", 5, vec![radio]);
        w.deezer
            .add_album(201, "Radioactive", "Imagine Dragons", 5, vec![single]);
        w.finder.results.lock().insert(
            "Порезы на руках".into(),
            vec![with(
                cand("Q1QvyIrUzz0", "youtube"),
                "Порезы на руках",
                "Юпи",
            )],
        );
        let text = "1. TVETH, BATO - Hella Quality\n2. Imagine Dragons - Radioactive [Night Visions]\n3. Юпи - Порезы на руках\n4. Nobody - Nothing At All\n5. garbage line";
        let created = w.importer.create("alice", text, true, None).unwrap();
        assert_eq!(created["status"], "queued");
        let id = as_str(created.get("id"));
        w.importer.run(&id).await;
        let report = w.importer.report(&id).unwrap();
        assert_eq!(report["status"], "completed");
        let r = rows(&report);
        let st = |i: usize| (as_str(r[i].get("status")), as_str(r[i].get("track_id")));
        assert_eq!(st(0), ("matched".into(), "deezer:4250555171".into()));
        assert_eq!(st(1), ("matched".into(), "deezer:63510358".into()));
        assert_eq!(r[1]["alternatives"][0]["id"], "deezer:74506420");
        assert_eq!(st(2), ("matched_youtube".into(), "yt:Q1QvyIrUzz0".into()));
        assert_eq!(
            (as_str(r[3].get("status")), as_str(r[4].get("status"))),
            ("not_found".into(), "invalid".into())
        );
        assert_eq!(r[0]["download"], "queued");
        assert_eq!(report["progress"], json!({"done": 5, "total": 5}));
        let liked: HashSet<String> = w
            .store
            .pending_stars(Some("alice"))
            .into_iter()
            .map(|(_, t, _)| t)
            .collect();
        assert_eq!(
            liked,
            HashSet::from([
                "deezer:4250555171".into(),
                "deezer:63510358".into(),
                "yt:Q1QvyIrUzz0".into()
            ])
        );
        // Import downloads use the background lane.
        let job = w.store.get_job("deezer:4250555171", "download").unwrap();
        assert_eq!(
            (job.priority, job.reason.as_deref()),
            (2, Some("like-import:alice"))
        );
    }

    #[tokio::test]
    async fn title_only_match_from_another_artist_is_not_taken() {
        let w = world().await;
        w.deezer.add_album(
            300,
            "If The Sky Came Down",
            "Lost Society",
            30,
            vec![dz_track(
                1725700,
                "Awake",
                "Lost Society",
                300,
                "If",
                1,
                &[],
            )],
        );
        w.finder.results.lock().insert(
            "Walking".into(),
            vec![with(
                cand("WWWWWWWWWWW", "youtube"),
                "Walking",
                "russianmafiaextra",
            )],
        );
        let id = as_str(
            w.importer
                .create(
                    "alice",
                    "russianmafiaextra - awake\nrussianmafiaextra - Walking",
                    false,
                    None,
                )
                .unwrap()
                .get("id"),
        );
        w.importer.run(&id).await;
        let r = rows(&w.importer.report(&id).unwrap());
        assert_eq!(as_str(r[0].get("status")), "not_found");
        assert!(!r[0].contains_key("track_id"));
        assert_eq!(as_str(r[1].get("status")), "matched_youtube");
        assert!(
            w.store.pending_stars(Some("alice")).is_empty(),
            "like=false"
        );
    }

    #[tokio::test]
    async fn recheck_takes_back_a_wrong_like_and_matches_again() {
        let w = world().await;
        let s = &w.store;
        w.deezer.add_album(
            300,
            "If The Sky Came Down",
            "Lost Society",
            30,
            vec![dz_track(
                1725700,
                "Awake",
                "Lost Society",
                300,
                "If",
                1,
                &[],
            )],
        );
        s.remember_track(
            "deezer:1725700",
            &m(json!({"title": "Awake", "artist": "Lost Society"})),
        );
        s.update_track(
            "deezer:1725700",
            &[
                Field::status("ready"),
                Field::LocalId(Some("WRONG1".into())),
            ],
        );
        let mut r = parse_text("russianmafiaextra - awake\nTVETH - Hella Quality").unwrap();
        r[0].extend(m(json!({"status": "fallback", "track_id": "deezer:1725700", "match": {"id": "deezer:1725700", "artist": "Lost Society", "title": "Awake", "score": 52.5}})));
        r[1].extend(m(json!({"status": "matched", "track_id": "deezer:4250555171", "match": {"artist": "TVETH, Bato", "title": "Hella Quality"}})));
        s.save_import(
            "old",
            "alice",
            "completed",
            &json!({"like": true, "playlist": null, "playlist_id": null}),
            &rows_value(&r),
        );
        let dry = w.importer.recheck("old", true).unwrap();
        assert_eq!(dry["wrong"], 1);
        assert!(s.pending_unstars("alice").is_empty());
        let done = w.importer.recheck("old", false).unwrap();
        assert_eq!(
            Value::Object(done),
            json!({"wrong": 1, "unstar": 1, "lines": dry["lines"]})
        );
        assert_eq!(s.pending_unstars("alice"), ["WRONG1"]);
        assert_eq!(w.importer.report("old").unwrap()["status"], "queued");
        w.importer.run("old").await;
        let statuses: Vec<String> = rows(&w.importer.report("old").unwrap())
            .iter()
            .map(|r| as_str(r.get("status")))
            .collect();
        assert_eq!(statuses, ["not_found", "matched"]);
        assert!(w.importer.recheck("missing", false).is_none());
    }

    #[tokio::test]
    async fn worker_runs_queued_imports_and_playlist_keeps_list_order() {
        let w = world().await;
        let dir = tempfile::tempdir().unwrap();
        w.deezer.add_album(
            102,
            "Third",
            "TVETH",
            1,
            vec![dz_track(3, "Third", "TVETH", 102, "Third", 1, &[])],
        );
        let text = "TVETH - Third\nTVETH - Hella Quality\nTVETH - Second";
        let id = as_str(
            w.importer
                .create("alice", text, true, Some(" Mix "))
                .unwrap()
                .get("id"),
        );
        w.importer.start();
        for _ in 0..200 {
            if w.importer.report(&id).unwrap()["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let report = w.importer.report(&id).unwrap();
        assert_eq!(report["status"], "completed");
        assert_eq!(report["options"], json!({"like": true, "playlist": "Mix"}));
        let ids: Vec<String> = rows(&report)
            .iter()
            .map(|r| as_str(r.get("track_id")))
            .collect();
        assert_eq!(ids, ["deezer:3", "deezer:4250555171", "deezer:2"]);
        // Downloaded and indexed in reverse order; the playlist follows the list.
        for (n, tid) in ids.iter().rev().enumerate() {
            let f = dir.path().join(format!("{n}.m4a"));
            std::fs::write(&f, b"x").unwrap();
            w.store.update_track(
                tid,
                &[
                    Field::status("ready"),
                    Field::File(Some(f.display().to_string())),
                    Field::LocalId(Some(format!("L{n}"))),
                ],
            );
        }
        w.importer.sync_playlists().await;
        assert!(
            w.navidrome.state.lock().playlists.is_empty(),
            "no credentials for alice yet"
        );
        assert!(
            w.nav
                .authenticate(&Params::parse("u=alice&p=alicepw", ""))
                .await
                .ok
        );
        w.importer.sync_playlists().await;
        let report = w.importer.report(&id).unwrap();
        let pid = as_str(report.get("playlist_id"));
        assert_eq!(w.navidrome.state.lock().playlists[&pid], ["L2", "L1", "L0"]);
        w.importer.sync_playlists().await;
        assert_eq!(
            w.navidrome.count("createPlaylist"),
            1,
            "unchanged playlists are not rewritten"
        );
        w.importer.stop().await;
    }
}

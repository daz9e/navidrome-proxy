//! Presenting external tracks to clients.
//!
//! The single rule: once a track is in the library *and* indexed by Navidrome, it is
//! listed as the real Navidrome song (its own id, likes, play counts, cover art).
//! Only tracks that are not local yet appear with a virtual `deezer:`/`yt:`/`sc:`
//! id. A client therefore never sees the same song twice under two ids.

use crate::db::{Field, Store, Track};
use crate::deezer;
use crate::identity::Identity;
use crate::ids;
use crate::navidrome::{Navidrome, NdError};
use crate::sources::YtmTrack;
use crate::subsonic::{models, params::Params};
use crate::util::{Json, as_str, truthy};
use futures::StreamExt;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SONG_TTL: Duration = Duration::from_secs(60);
const SONG_CACHE_MAX: usize = 20_000;

pub struct Catalog {
    pub store: Arc<Store>,
    pub identity: Identity,
    pub navidrome: Arc<Navidrome>,
    song_cache: Mutex<HashMap<(String, String), (Instant, Json)>>,
}

impl Catalog {
    pub fn new(store: Arc<Store>, identity: Identity, navidrome: Arc<Navidrome>) -> Self {
        Self {
            store,
            identity,
            navidrome,
            song_cache: Mutex::new(HashMap::new()),
        }
    }

    // ------------------------------------------------------------ remember
    /// Store Deezer rows; a search row never overwrites a complete (hydrated) record.
    pub fn remember_deezer(&self, rows: &[Json], album: Option<&Json>) -> Vec<String> {
        let with_id: Vec<(&Json, String)> = rows
            .iter()
            .filter(|r| truthy(r.get("id")))
            .map(|r| (r, ids::track(as_str(r.get("id")))))
            .collect();
        let known = self
            .store
            .get_tracks(with_id.iter().map(|(_, id)| id.as_str()));
        let mut items = Vec::new();
        for (row, track_id) in &with_id {
            let mut meta = deezer::track_meta(row, album);
            if !truthy(meta.get("full")) {
                if known
                    .get(track_id)
                    .is_some_and(|t| truthy(t.meta.get("full")))
                {
                    continue;
                }
                meta.remove("full");
            }
            items.push((track_id.clone(), meta));
        }
        self.store.remember_tracks(&items);
        with_id.into_iter().map(|(_, id)| id).collect()
    }

    pub fn remember_ytmusic(&self, rows: &[YtmTrack]) -> Vec<String> {
        let items: Vec<(String, Json)> = rows
            .iter()
            .map(|row| {
                let artist = if row.artists.is_empty() { "YouTube".to_string() } else { row.artists.join(", ") };
                let meta = json!({
                    "source": "youtube", "title": row.title, "artists": row.artists, "artist": artist,
                    "album": row.album, "duration": row.duration, "cover": row.thumbnail, "explicit": row.explicit,
                });
                (ids::youtube(&row.video_id), meta.as_object().cloned().unwrap_or_default())
            })
            .collect();
        self.store.remember_tracks(&items);
        items.into_iter().map(|(id, _)| id).collect()
    }

    // ------------------------------------------------------------- present
    pub fn starred_map(&self, username: &str) -> HashMap<String, f64> {
        self.store
            .pending_stars(Some(username))
            .into_iter()
            .map(|(_, target, created)| (target, created))
            .collect()
    }

    pub fn virtual_song(&self, requested_id: &str, track: &Track, starred: Option<f64>) -> Json {
        let empty = Json::new();
        let audio = if track.is_ready() {
            &track.audio
        } else {
            &empty
        };
        models::song(
            requested_id,
            &track.meta,
            audio,
            Some(&json!(track.created_at)),
            starred,
        )
    }

    /// Navidrome song objects (with the caller's stars) for local ids, briefly cached.
    pub async fn navidrome_songs(
        &self,
        local_ids: &[String],
        creds: &Params,
    ) -> HashMap<String, Json> {
        let user = creds.username().to_string();
        let now = Instant::now();
        let mut out = HashMap::new();
        let mut missing = Vec::new();
        {
            let cache = self.song_cache.lock();
            let mut seen = HashSet::new();
            for local_id in local_ids.iter().filter(|i| seen.insert(i.as_str())) {
                match cache.get(&(user.clone(), local_id.clone())) {
                    Some((expires, song)) if *expires > now => {
                        out.insert(local_id.clone(), song.clone());
                    }
                    _ => missing.push(local_id.clone()),
                }
            }
        }
        let fetched: Vec<(String, Option<Json>)> = futures::stream::iter(missing)
            .map(|local_id| async move {
                let song = match self
                    .navidrome
                    .call("getSong", creds, &[("id", &local_id)])
                    .await
                {
                    Ok(root) => root.get("song").and_then(Value::as_object).cloned(),
                    Err(NdError::Upstream { code: 70, .. }) => {
                        // The song vanished from Navidrome: forget the stale mapping.
                        for track in self.store.tracks_by_local_ids([local_id.as_str()]).values() {
                            self.store.update_track(&track.id, &[Field::LocalId(None)]);
                        }
                        None
                    }
                    Err(_) => None,
                };
                (local_id, song)
            })
            .buffer_unordered(8)
            .collect()
            .await;
        let mut cache = self.song_cache.lock();
        if cache.len() > SONG_CACHE_MAX {
            cache.clear();
        }
        for (local_id, song) in fetched {
            if let Some(song) = song {
                cache.insert(
                    (user.clone(), local_id.clone()),
                    (now + SONG_TTL, song.clone()),
                );
                out.insert(local_id, song);
            }
        }
        out
    }

    pub fn forget_song(&self, username: &str, local_id: &str) {
        self.song_cache
            .lock()
            .remove(&(username.to_string(), local_id.to_string()));
    }

    /// Songs for external ids, local objects where possible, deduplicated, order kept.
    pub async fn present(
        &self,
        track_ids: &[String],
        creds: &Params,
        exclude_local: &HashSet<String>,
    ) -> Vec<Json> {
        let canonical = self.identity.canonical_many(track_ids);
        let wanted: HashSet<&str> = canonical
            .values()
            .map(String::as_str)
            .chain(track_ids.iter().map(String::as_str))
            .collect();
        let tracks = self.store.get_tracks(wanted);
        let local_ids: Vec<String> = tracks
            .values()
            .filter(|t| t.is_ready())
            .filter_map(|t| t.local_id.clone())
            .collect();
        let local_songs = if local_ids.is_empty() {
            HashMap::new()
        } else {
            self.navidrome_songs(&local_ids, creds).await
        };
        let starred = self.starred_map(creds.username());
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for track_id in track_ids {
            let Some(track) = canonical
                .get(track_id)
                .and_then(|c| tracks.get(c))
                .or_else(|| tracks.get(track_id))
            else {
                continue;
            };
            if let Some(song) = track.local_id.as_ref().and_then(|l| local_songs.get(l)) {
                let local_id = track.local_id.clone().unwrap_or_default();
                if !exclude_local.contains(&local_id) && seen.insert(local_id) {
                    out.push(song.clone());
                }
                continue;
            }
            // Not indexed yet: keep the id the caller listed, deduplicate by recording.
            if !seen.insert(track.id.clone()) {
                continue;
            }
            let star = starred
                .get(track_id)
                .or_else(|| starred.get(&track.id))
                .copied();
            out.push(self.virtual_song(track_id, track, star));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeNavidrome, dz_track, m, nd_song, serve};

    async fn world() -> (
        Catalog,
        FakeNavidrome,
        crate::testutil::Server,
        tempfile::TempDir,
    ) {
        let fake = FakeNavidrome::default();
        fake.add_song(nd_song("L1", "Hella Quality", "TVETH", "RAW", "al1"));
        let server = serve(fake.router()).await;
        let store = Arc::new(Store::memory());
        let catalog = Catalog::new(
            store.clone(),
            Identity::new(store),
            Arc::new(Navidrome::new(&server.url, 60.0)),
        );
        (catalog, fake, server, tempfile::tempdir().unwrap())
    }

    fn creds() -> Params {
        Params::parse("u=alice&p=alicepw", "")
    }

    #[tokio::test]
    async fn search_rows_never_overwrite_hydrated_records() {
        let (c, ..) = world().await;
        let full = dz_track(
            1,
            "Hella Quality",
            "TVETH",
            100,
            "RAW",
            1,
            &["TVETH", "Bato"],
        );
        let mut row = full.clone();
        row.remove("contributors");
        row.insert("title".into(), json!("changed"));
        assert_eq!(c.remember_deezer(&[full], None), ["deezer:1"]);
        assert_eq!(
            c.remember_deezer(&[row.clone(), m(json!({"title": "no id"}))], None),
            ["deezer:1"]
        );
        let t = c.store.get_track("deezer:1").unwrap();
        assert_eq!(
            (t.title(), t.meta["full"].clone()),
            ("Hella Quality".into(), json!(true))
        );
        c.remember_deezer(
            &[{
                let mut r = row;
                r.insert("id".into(), json!(2));
                r
            }],
            None,
        );
        assert!(
            !c.store
                .get_track("deezer:2")
                .unwrap()
                .meta
                .contains_key("full"),
            "search rows are not marked"
        );
        let yt = YtmTrack {
            video_id: "sO4pSj466Hk".into(),
            title: "Кошмар".into(),
            ..Default::default()
        };
        assert_eq!(c.remember_ytmusic(&[yt]), ["yt:sO4pSj466Hk"]);
        assert_eq!(
            c.store.get_track("yt:sO4pSj466Hk").unwrap().artist(),
            "YouTube"
        );
    }

    #[tokio::test]
    async fn present_prefers_local_songs_and_deduplicates() {
        let (c, fake, _s, dir) = world().await;
        let file = dir.path().join("a.m4a");
        std::fs::write(&file, b"x").unwrap();
        c.store.remember_track(
            "deezer:1",
            &m(json!({"title": "Hella Quality", "artist": "TVETH"})),
        );
        c.store.update_track(
            "deezer:1",
            &[
                Field::status("ready"),
                Field::File(Some(file.display().to_string())),
                Field::LocalId(Some("L1".into())),
            ],
        );
        c.store.remember_track(
            "yt:sO4pSj466Hk",
            &m(json!({"title": "Hella Quality", "artist": "TVETH"})),
        );
        c.identity.link("yt:sO4pSj466Hk", "deezer:1", "source");
        c.store.remember_track(
            "deezer:2",
            &m(json!({"title": "Second", "artist": "TVETH"})),
        );
        c.store.add_pending_star("alice", "deezer:2");
        let list = [
            "deezer:1",
            "yt:sO4pSj466Hk",
            "deezer:2",
            "deezer:2",
            "deezer:404",
        ]
        .map(String::from);
        let songs = c.present(&list, &creds(), &HashSet::new()).await;
        let ids: Vec<&str> = songs.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["L1", "deezer:2"]);
        assert!(songs[1].contains_key("starred"));
        let excluded = c
            .present(&list, &creds(), &HashSet::from(["L1".to_string()]))
            .await;
        assert_eq!(excluded.len(), 1);
        assert_eq!(fake.count("getSong"), 1, "cached for a minute");
        // A song that vanished from Navidrome loses its stale mapping.
        c.forget_song("alice", "L1");
        fake.state.lock().songs.clear();
        let songs = c.present(&list, &creds(), &HashSet::new()).await;
        assert_eq!(songs[0]["id"], "deezer:1");
        assert_eq!(c.store.get_track("deezer:1").unwrap().local_id, None);
    }
}

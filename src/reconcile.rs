//! Link YouTube/SoundCloud-only downloads to the catalog once Deezer knows them.
//!
//! Only an alias is added (`deezer:<id>` -> the downloaded track): search results,
//! albums and likes for the Deezer id then resolve to the existing file. Tags are
//! left alone on purpose — retagging title/album would give the song a new Navidrome
//! id and silently drop its likes and play counts.

use crate::db::{Store, Track};
use crate::deezer::{self, Deezer};
use crate::identity::Identity;
use crate::ids;
use crate::matching::{self, Wanted};
use crate::util::{as_f64, as_str};
use parking_lot::Mutex;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

pub const MIN_SCORE: f64 = 85.0;

pub struct Reconciler {
    store: Arc<Store>,
    identity: Identity,
    deezer: Arc<Deezer>,
    interval: Duration,
    batch: usize,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Duration of what is actually in the file, else the catalog one.
pub fn known_duration(track: &Track) -> f64 {
    as_f64(track.audio.get("duration")).filter(|d| *d != 0.0).unwrap_or_else(|| track.duration())
}

impl Reconciler {
    pub fn new(store: Arc<Store>, identity: Identity, deezer: Arc<Deezer>, interval: f64, batch: usize) -> Self {
        Self { store, identity, deezer, interval: Duration::from_secs_f64(interval), batch, task: Mutex::new(None) }
    }

    pub fn start(self: &Arc<Self>) {
        let me = self.clone();
        *self.task.lock() = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(120)).await;
            loop {
                me.run_once().await;
                tokio::time::sleep(me.interval).await;
            }
        }));
    }

    pub async fn stop(&self) {
        let task = self.task.lock().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    /// One pass over unlinked downloads; returns how many got a catalog alias.
    pub async fn run_once(&self) -> usize {
        deezer::BACKGROUND.scope(true, self.pass()).await
    }

    async fn pass(&self) -> usize {
        let (mut linked, mut checked) = (0, 0);
        for track in self.store.tracks_with_status(&["ready"]) {
            if !(track.id.starts_with("yt:") || track.id.starts_with("sc:")) {
                continue;
            }
            if self.store.aliases_of(&track.id).iter().any(|(alias, _)| alias.starts_with("deezer:")) {
                continue;
            }
            let key = format!("reconciled:{}", track.id);
            if self.store.cache_get(&key).is_some() {
                continue;
            }
            checked += 1;
            if checked > self.batch {
                break;
            }
            let artists = track.artists();
            let query = format!("{} {}", artists.first().map(String::as_str).unwrap_or(""), track.title());
            let Ok(rows) = self.deezer.search_tracks(&query, 10, 0).await else { continue };
            let duration = known_duration(&track);
            let wanted = Wanted { title: track.title(), artists, duration: Some(duration), ..Default::default() };
            let mut best: Option<(f64, String)> = None;
            for row in &rows {
                let meta = deezer::track_meta(row, None);
                let score = matching::score_catalog_candidate(&wanted, &meta).score;
                let close = (as_f64(meta.get("duration")).unwrap_or(0.0) - duration).abs() <= 3.0;
                if score >= MIN_SCORE && close && best.as_ref().is_none_or(|(s, _)| score > *s) {
                    best = Some((score, as_str(row.get("id"))));
                }
            }
            if let Some((score, id)) = best {
                let deezer_id = ids::track(id);
                if self.identity.link(&deezer_id, &track.id, "reconciler") {
                    linked += 1;
                    tracing::info!("linked {} to catalog {deezer_id} (score {score:.0})", track.id);
                }
            }
            // Check again in a week (new releases reach Deezer late).
            self.store.cache_set(&key, &json!(crate::util::now() as i64), 7.0 * 86400.0);
        }
        linked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Field;
    use crate::testutil::{FakeDeezer, dz_track, m, serve};

    #[tokio::test]
    async fn youtube_download_is_linked_to_its_later_deezer_release() {
        let fake = FakeDeezer::empty();
        let with_duration = |id, album_id, album, secs| {
            let mut t = dz_track(id, "Brand New", "TVETH", album_id, album, 1, &[]);
            t.insert("duration".into(), json!(secs));
            t
        };
        fake.add_album(300, "Brand New", "TVETH", 1, vec![with_duration(3001, 300, "Brand New", 122)]);
        fake.add_album(301, "Other", "TVETH", 1, vec![with_duration(3002, 301, "Other", 180)]);
        let server = serve(fake.router()).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("song.m4a");
        std::fs::write(&file, b"x").unwrap();
        let store = Arc::new(Store::memory());
        for (id, title, artist, duration, audio) in
            [("yt:AAAAAAAAAAA", "Brand New", "TVETH", 120, 121), ("yt:BBBBBBBBBBB", "Unknown", "Nobody", 100, 100)]
        {
            store.remember_track(id, &m(json!({"title": title, "artists": [artist], "duration": duration})));
            store.update_track(
                id,
                &[Field::status("ready"), Field::File(Some(file.display().to_string())), Field::Audio(Some(m(json!({"duration": audio}))))],
            );
        }
        let identity = Identity::new(store.clone());
        let deezer = Arc::new(Deezer::new(store.clone(), 100.0, &server.url, 60.0));
        let r = Reconciler::new(store.clone(), Identity::new(store.clone()), deezer, 3600.0, 50);
        assert_eq!(r.run_once().await, 1);
        assert_eq!(identity.canonical("deezer:3001"), "yt:AAAAAAAAAAA");
        assert_eq!(identity.canonical("deezer:3002"), "deezer:3002", "different length: another version");
        assert_eq!(r.run_once().await, 0, "remembered, not rechecked immediately");
        assert!(store.cache_get("reconciled:yt:BBBBBBBBBBB").is_some());
    }

    #[tokio::test]
    async fn batch_limits_one_pass() {
        let fake = FakeDeezer::empty();
        let server = serve(fake.router()).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("song.m4a");
        std::fs::write(&file, b"x").unwrap();
        let store = Arc::new(Store::memory());
        for id in ["yt:AAAAAAAAAAA", "yt:BBBBBBBBBBB", "yt:CCCCCCCCCCC"] {
            store.remember_track(id, &m(json!({"title": id, "artists": ["A"], "duration": 100})));
            store.update_track(id, &[Field::status("ready"), Field::File(Some(file.display().to_string()))]);
        }
        let deezer = Arc::new(Deezer::new(store.clone(), 100.0, &server.url, 60.0));
        let r = Reconciler::new(store.clone(), Identity::new(store.clone()), deezer, 3600.0, 2);
        r.run_once().await;
        assert_eq!(fake.state.lock().hits.len(), 2);
        r.run_once().await;
        assert_eq!(fake.state.lock().hits.len(), 3);
    }
}

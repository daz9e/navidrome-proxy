//! User intents that wait for a track to become local, then land in Navidrome.
//!
//! Navidrome owns likes, scrobbles and playlists. For an external track the proxy
//! keeps the intent (per user) until the download is indexed, then replays it with
//! that user's credentials and forgets it. Credentials are never persisted: an
//! intent whose user has not been seen since a restart waits for their next request.

use crate::db::{Store, Track};
use crate::identity::Identity;
use crate::ids::{self, Kind};
use crate::matching;
use crate::navidrome::{self, NdError, Navidrome};
use crate::subsonic::params::Params;
use crate::tasks::Background;
use crate::util::as_str;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Flushed {
    pub stars: usize,
    pub unstars: usize,
    pub scrobbles: usize,
    pub playlist: usize,
}

pub struct Intents {
    store: Arc<Store>,
    identity: Identity,
    navidrome: Arc<Navidrome>,
    min_interval: Duration,
    last_flush: Mutex<HashMap<String, Instant>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    pub background: Background,
}

type Step = Result<usize, NdError>;

impl Intents {
    pub fn new(store: Arc<Store>, identity: Identity, navidrome: Arc<Navidrome>, min_interval: f64) -> Self {
        Self {
            store,
            identity,
            navidrome,
            min_interval: Duration::from_secs_f64(min_interval),
            last_flush: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            background: Background::default(),
        }
    }

    fn local_id(&self, target: &str) -> Option<String> {
        let track = self.store.get_track(&self.identity.canonical(target))?;
        if track.is_ready() { track.local_id } else { None }
    }

    /// Schedule a flush for `username` (rate limited) if they have pending work.
    pub fn maybe_flush(self: &Arc<Self>, username: &str, force: bool) {
        {
            let mut last = self.last_flush.lock();
            if !force && last.get(username).is_some_and(|t| t.elapsed() < self.min_interval) {
                return;
            }
            last.insert(username.to_string(), Instant::now());
        }
        if !self.store.users_with_pending_work().contains(username) {
            return;
        }
        let (me, user) = (self.clone(), username.to_string());
        self.background.spawn(async move {
            me.flush(&user).await;
        });
    }

    /// A download got its Navidrome id: replay every user's pending intents.
    pub async fn on_track_mapped(&self, _track: Track) {
        for username in self.store.users_with_pending_work() {
            self.flush(&username).await;
        }
    }

    pub async fn flush(&self, username: &str) -> Flushed {
        let mut done = Flushed::default();
        let Some(creds) = self.navidrome.credentials_for(username) else { return done };
        let lock = self.locks.lock().entry(username.to_string()).or_default().clone();
        {
            let _guard = lock.lock().await;
            let result: Result<(), NdError> = async {
                done.unstars = self.unstars(username, &creds).await?;
                done.stars = self.stars(username, &creds).await?;
                done.scrobbles = self.scrobbles(username, &creds).await?;
                done.playlist = self.playlists(username, &creds).await?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                tracing::info!("intent flush for {username} postponed: {e}");
            }
        }
        if done != Flushed::default() {
            tracing::info!("synced intents of {username} to Navidrome: {done:?}");
        }
        done
    }

    async fn unstars(&self, username: &str, creds: &Params) -> Step {
        let local_ids = self.store.pending_unstars(username);
        if local_ids.is_empty() {
            return Ok(0);
        }
        match self.navidrome.call_many("unstar", creds, &[("id", local_ids.clone())], &[]).await {
            Err(e @ NdError::Upstream { .. }) => {
                tracing::warn!("unstar of {} songs for {username} rejected: {e}", local_ids.len());
                Ok(0)
            }
            Err(e) => Err(e),
            Ok(_) => {
                self.store.remove_pending_unstars(username, &local_ids);
                Ok(local_ids.len())
            }
        }
    }

    async fn stars(&self, username: &str, creds: &Params) -> Step {
        let (mut songs, mut albums, mut artists) = (Vec::new(), Vec::new(), Vec::new());
        for (_, target, _) in self.store.pending_stars(Some(username)) {
            let Some(vid) = ids::parse(&target) else {
                self.store.remove_pending_star(username, &target);
                continue;
            };
            if vid.is_track() {
                if let Some(local) = self.local_id(&target) {
                    songs.push((target, local));
                }
            } else if vid.kind == Kind::Album {
                if let Some(local) = self.local_album(&vid.key, creds).await? {
                    albums.push((target, local));
                }
            } else if vid.kind == Kind::Artist
                && let Some(local) = self.local_artist(&vid.key, creds).await?
            {
                artists.push((target, local));
            }
        }
        let mut synced = 0;
        for (key, pairs) in [("id", songs), ("albumId", albums), ("artistId", artists)] {
            if pairs.is_empty() {
                continue;
            }
            let locals: Vec<String> = pairs.iter().map(|(_, l)| l.clone()).collect();
            match self.navidrome.call_many("star", creds, &[(key, locals)], &[]).await {
                Err(e @ NdError::Upstream { .. }) => {
                    tracing::warn!("star of {pairs:?} for {username} rejected: {e}");
                    continue;
                }
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            for (target, _) in pairs {
                self.store.remove_pending_star(username, &target);
                synced += 1;
            }
        }
        Ok(synced)
    }

    /// The Navidrome album holding any downloaded track of a Deezer album.
    pub async fn local_album(&self, deezer_album_id: &str, creds: &Params) -> Result<Option<String>, NdError> {
        for track in self.store.tracks_in_album(deezer_album_id) {
            let track = self.store.get_track(&self.identity.canonical(&track.id)).unwrap_or(track);
            let Some(local_id) = track.local_id.clone().filter(|_| track.is_ready()) else { continue };
            match self.navidrome.call("getSong", creds, &[("id", &local_id)]).await {
                Ok(root) => {
                    let album_id = as_str(root.get("song").and_then(|s| s.get("albumId")));
                    if !album_id.is_empty() {
                        return Ok(Some(album_id));
                    }
                }
                Err(NdError::Upstream { .. }) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    async fn local_artist(&self, deezer_artist_id: &str, creds: &Params) -> Result<Option<String>, NdError> {
        let Some(track) = self.store.ready_track_by_artist_id(deezer_artist_id) else { return Ok(None) };
        let Some(name) = track.artists().into_iter().next() else { return Ok(None) };
        let root = match self
            .navidrome
            .call("search3", creds, &[("query", &name), ("artistCount", "10"), ("albumCount", "0"), ("songCount", "0")])
            .await
        {
            Ok(root) => root,
            Err(NdError::Upstream { .. }) => return Ok(None),
            Err(e) => return Err(e),
        };
        let artists = crate::util::arr(root.get("searchResult3").and_then(|r| r.get("artist")));
        let wanted = matching::normalize(&name);
        Ok(artists
            .iter()
            .filter_map(Value::as_object)
            .find(|a| matching::normalize(&as_str(a.get("name"))) == wanted)
            .map(navidrome::id_of))
    }

    async fn scrobbles(&self, username: &str, creds: &Params) -> Step {
        let mut sent = 0;
        for (track_id, when, submission) in self.store.pending_scrobbles(username) {
            let Some(local_id) = self.local_id(&track_id) else {
                if crate::util::now() - when as f64 / 1000.0 > 30.0 * 86400.0 {
                    self.store.remove_pending_scrobble(username, &track_id, when, submission);
                }
                continue;
            };
            let extra = [("id", local_id.as_str()), ("time", &when.to_string()), ("submission", if submission { "true" } else { "false" })];
            match self.navidrome.call("scrobble", creds, &extra).await {
                Err(e @ NdError::Upstream { .. }) => tracing::warn!("scrobble of {track_id} for {username} rejected: {e}"),
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            self.store.remove_pending_scrobble(username, &track_id, when, submission);
            sent += 1;
        }
        Ok(sent)
    }

    async fn playlists(&self, username: &str, creds: &Params) -> Step {
        let mut added = 0;
        for (playlist_id, track_id) in self.store.pending_playlist_adds(username) {
            let Some(local_id) = self.local_id(&track_id) else { continue };
            match self.navidrome.call("updatePlaylist", creds, &[("playlistId", &playlist_id), ("songIdToAdd", &local_id)]).await {
                Err(e @ NdError::Upstream { .. }) => {
                    tracing::warn!("playlist {playlist_id} add of {track_id} for {username} rejected: {e}")
                }
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            self.store.remove_pending_playlist_add(username, &playlist_id, &track_id);
            added += 1;
        }
        Ok(added)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Field;
    use crate::testutil::{FakeNavidrome, m, nd_song, serve};
    use serde_json::json;

    async fn world() -> (Arc<Intents>, FakeNavidrome, crate::testutil::Server, tempfile::TempDir) {
        let fake = FakeNavidrome::default();
        fake.add_song(nd_song("L1", "Hella Quality", "TVETH", "RAW", "al1"));
        let server = serve(fake.router()).await;
        let store = Arc::new(Store::memory());
        let nav = Arc::new(Navidrome::new(&server.url, 60.0));
        let intents = Arc::new(Intents::new(store.clone(), Identity::new(store), nav, 20.0));
        (intents, fake, server, tempfile::tempdir().unwrap())
    }

    fn make_local(i: &Intents, dir: &std::path::Path, id: &str, local: &str) {
        let f = dir.join(format!("{}.m4a", id.replace(':', "_")));
        std::fs::write(&f, b"x").unwrap();
        i.store.remember_track(id, &m(json!({"title": "Hella Quality", "artists": ["TVETH"], "album_id": "100", "artist_id": "1"})));
        i.store.update_track(id, &[Field::status("ready"), Field::File(Some(f.display().to_string())), Field::LocalId(Some(local.into()))]);
    }

    #[tokio::test]
    async fn waits_for_credentials_then_replays_everything() {
        let (i, fake, _s, dir) = world().await;
        let s = &i.store;
        s.add_pending_star("alice", "deezer:1");
        s.add_pending_star("alice", "deezer-album:100");
        s.add_pending_star("alice", "deezer-artist:1");
        s.add_pending_star("alice", "garbage");
        s.add_pending_scrobble("alice", "deezer:1", (crate::util::now() * 1000.0) as i64, true);
        s.add_pending_playlist_add("alice", "pl1", "deezer:1");
        fake.state.lock().playlists.insert("pl1".into(), vec![]);
        assert_eq!(i.flush("alice").await, Flushed::default(), "no credentials seen yet");
        let nav = i.navidrome.clone();
        assert!(nav.authenticate(&Params::parse("u=alice&p=alicepw", "")).await.ok);
        assert_eq!(i.flush("alice").await, Flushed::default(), "not downloaded yet: kept");
        assert_eq!(s.pending_stars(Some("alice")).len(), 3, "invalid targets are dropped");
        make_local(&i, dir.path(), "deezer:1", "L1");
        i.on_track_mapped(s.get_track("deezer:1").unwrap()).await;
        assert!(s.users_with_pending_work().is_empty(), "{:?}", s.pending_stars(None));
        let st = fake.state.lock();
        assert!(st.stars["alice"].contains("L1"));
        assert_eq!(st.scrobbles, [("alice".to_string(), "L1".to_string(), "true".to_string())]);
        assert_eq!(st.playlists["pl1"], ["L1"]);
        let star_calls: Vec<String> = fake_requests(&st, "star");
        assert!(star_calls.iter().any(|q| q.contains("albumId=al1")) && star_calls.iter().any(|q| q.contains("artistId=")));
    }

    fn fake_requests(st: &crate::testutil::NavidromeState, method: &str) -> Vec<String> {
        st.requests.iter().filter(|(m, _)| m == method).map(|(_, q)| q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")).collect()
    }

    #[tokio::test]
    async fn unstars_rejections_and_old_scrobbles() {
        let (i, fake, _s, dir) = world().await;
        assert!(i.navidrome.authenticate(&Params::parse("u=alice&p=alicepw", "")).await.ok);
        fake.state.lock().stars.entry("alice".into()).or_default().insert("L1".into());
        i.store.add_pending_unstar("alice", "L1");
        i.store.add_pending_scrobble("alice", "deezer:9", 1000, true);
        make_local(&i, dir.path(), "deezer:2", "GONE");
        i.store.add_pending_star("alice", "deezer:2");
        let done = i.flush("alice").await;
        assert_eq!(done.unstars, 1);
        assert_eq!(done.stars, 0, "Navidrome refused the star: kept for later");
        assert!(fake.state.lock().stars["alice"].is_empty());
        assert!(i.store.pending_scrobbles("alice").is_empty(), "month-old scrobbles of missing tracks expire");
        assert_eq!(i.store.pending_stars(Some("alice")).len(), 1);
    }

    #[tokio::test]
    async fn maybe_flush_is_rate_limited() {
        let (i, fake, _s, _dir) = world().await;
        assert!(i.navidrome.authenticate(&Params::parse("u=alice&p=alicepw", "")).await.ok);
        i.store.add_pending_unstar("alice", "L1");
        i.maybe_flush("alice", false);
        for _ in 0..50 {
            if i.store.pending_unstars("alice").is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(i.store.pending_unstars("alice").is_empty());
        i.store.add_pending_unstar("alice", "L1");
        i.maybe_flush("alice", false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(i.store.pending_unstars("alice").len(), 1, "within the interval");
        assert_eq!(fake.count("unstar"), 1);
    }
}

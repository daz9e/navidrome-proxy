//! One recording, many ids.
//!
//! The same song reaches the proxy as a Deezer track, a YouTube Music video, a
//! SoundCloud upload, and sometimes under a slightly different title ("Юпи - Порезы
//! на руках" vs "Порезы на руках"). Without a shared identity each of those would be
//! downloaded again and listed twice.
//!
//! Every downloaded track has one canonical id (the id it was first requested as).
//! Other ids of the same recording become *aliases* of it:
//!
//! * exact aliases – the source a download actually came from (a Deezer track fetched
//!   from YouTube video V makes `yt:V` an alias), the ISRC, and ids linked by the
//!   catalog reconciler;
//! * recording keys – "primary artist | base title" plus duration (±3 s), which finds
//!   the same song across catalogs without any shared id.

use crate::db::{Store, Track};
use crate::util::{Json, as_f64, as_str};
use crate::{ids, matching};
use std::collections::HashMap;
use std::sync::Arc;

pub const DURATION_SLACK: f64 = 3.0;

pub fn recording_key(artists: &[String], title: &str) -> Option<String> {
    let lead = matching::normalize(artists.first().map(String::as_str).unwrap_or(""));
    let base = matching::base_title(title);
    (!lead.is_empty() && !base.is_empty()).then(|| format!("{lead}|{base}"))
}

/// The artists of a meta dict: its `artists` list, else its `artist` credit split.
pub fn meta_artist_list(meta: &Json) -> Vec<String> {
    match meta.get("artists") {
        Some(serde_json::Value::Array(a)) if !a.is_empty() => {
            a.iter().map(|v| as_str(Some(v))).collect()
        }
        _ => matching::split_artists(&as_str(meta.get("artist"))),
    }
}

#[derive(Clone)]
pub struct Identity {
    pub store: Arc<Store>,
}

impl Identity {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    /// Follow the alias chain (at most a few hops) to the canonical id.
    pub fn canonical(&self, track_id: &str) -> String {
        let mut seen = vec![track_id.to_string()];
        let mut current = track_id.to_string();
        for _ in 0..5 {
            match self.store.resolve_alias(&current) {
                Some(next) if !seen.contains(&next) => {
                    seen.push(next.clone());
                    current = next;
                }
                _ => return current,
            }
        }
        current
    }

    pub fn canonical_many(&self, track_ids: &[String]) -> HashMap<String, String> {
        let mapping = self
            .store
            .resolve_aliases(track_ids.iter().map(String::as_str));
        track_ids
            .iter()
            .map(|t| {
                (
                    t.clone(),
                    mapping
                        .get(t)
                        .map(|m| self.canonical(m))
                        .unwrap_or_else(|| t.clone()),
                )
            })
            .collect()
    }

    pub fn link(&self, alias: &str, canonical: &str, how: &str) -> bool {
        let target = self.canonical(canonical);
        if self.canonical(alias) == target {
            return false;
        }
        self.store.add_alias(alias, &target, how)
    }

    /// Record the identities of a (downloaded) track.
    pub fn register(&self, track: &Track) {
        let isrc = as_str(track.meta.get("isrc"));
        if !isrc.is_empty() {
            self.store
                .add_alias(&format!("isrc:{}", isrc.to_uppercase()), &track.id, "isrc");
        }
        // Same artist list as `find_existing` uses, so a credit string keys identically.
        if let Some(key) = recording_key(&meta_artist_list(&track.meta), &track.title()) {
            let duration = as_f64(track.audio.get("duration"))
                .filter(|d| *d != 0.0)
                .unwrap_or(track.duration());
            self.store.add_recording(&key, &track.id, duration);
        }
        if let Some(source_id) = Self::source_id(track)
            && source_id != track.id
        {
            self.store.add_alias(
                &source_id,
                &track.id,
                &format!("source:{}", track.source.as_deref().unwrap_or("")),
            );
        }
    }

    pub fn source_id(track: &Track) -> Option<String> {
        let reference = track.source_ref.as_deref().unwrap_or("");
        match track.source.as_deref() {
            Some("youtube") if ids::valid_youtube_id(reference) => Some(ids::youtube(reference)),
            Some("soundcloud")
                if !reference.is_empty() && reference.bytes().all(|b| b.is_ascii_digit()) =>
            {
                Some(ids::soundcloud(reference))
            }
            _ => None,
        }
    }

    /// A known track that is the same recording as `meta` (ISRC, then key+duration).
    pub fn find_existing(&self, meta: &Json, exclude: Option<&str>, usable: bool) -> Option<Track> {
        let mut candidates = Vec::new();
        let isrc = as_str(meta.get("isrc"));
        if !isrc.is_empty()
            && let Some(hit) = self
                .store
                .resolve_alias(&format!("isrc:{}", isrc.to_uppercase()))
        {
            candidates.push(hit);
        }
        let duration = as_f64(meta.get("duration")).unwrap_or(0.0);
        if let Some(key) = recording_key(&meta_artist_list(meta), &as_str(meta.get("title"))) {
            for (track_id, known) in self.store.recordings(&key) {
                if duration != 0.0 && known != 0.0 && (known - duration).abs() > DURATION_SLACK {
                    continue;
                }
                candidates.push(track_id);
            }
        }
        let mut seen = std::collections::HashSet::new();
        for track_id in candidates.into_iter().filter(|c| seen.insert(c.clone())) {
            let canonical = self.canonical(&track_id);
            if Some(canonical.as_str()) == exclude {
                continue;
            }
            if let Some(track) = self.store.get_track(&canonical)
                && (!usable || track.is_ready())
            {
                return Some(track);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Field;
    use serde_json::json;

    struct World {
        store: Arc<Store>,
        identity: Identity,
        music: tempfile::TempDir,
    }

    fn world() -> World {
        let store = Arc::new(Store::memory());
        World {
            identity: Identity::new(store.clone()),
            store,
            music: tempfile::tempdir().unwrap(),
        }
    }

    fn ready(
        w: &World,
        id: &str,
        meta: serde_json::Value,
        source: Option<&str>,
        reference: Option<&str>,
    ) -> Track {
        let f = w.music.path().join(format!("{}.m4a", id.replace(':', "-")));
        std::fs::write(&f, b"x").unwrap();
        let meta = meta.as_object().cloned().unwrap();
        let duration = meta.get("duration").cloned().unwrap_or(json!(0));
        w.store.remember_track(id, &meta);
        w.store.update_track(
            id,
            &[
                Field::status("ready"),
                Field::File(Some(f.display().to_string())),
                Field::Source(source.map(String::from)),
                Field::SourceRef(reference.map(String::from)),
                Field::Audio(json!({"duration": duration}).as_object().cloned()),
            ],
        );
        w.store.get_track(id).unwrap()
    }

    fn m(v: serde_json::Value) -> Json {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn recording_key_ignores_feat_brackets_and_collab_order() {
        assert_eq!(
            recording_key(&["Юпи".into()], "Порезы на руках").as_deref(),
            Some("юпи|порезы на руках")
        );
        let artists = meta_artist_list(&m(json!({"artist": "TVETH, Bato"})));
        assert_eq!(
            recording_key(&artists, "Hella Quality (feat. Bato)").as_deref(),
            Some("tveth|hella quality")
        );
        assert_eq!(recording_key(&[], "x"), None);
    }

    #[test]
    fn download_source_becomes_alias() {
        let w = world();
        let t = ready(
            &w,
            "deezer:4274804272",
            json!({"title": "Порезы на руках", "artists": ["Юпи"], "duration": 124, "isrc": "ru123"}),
            Some("youtube"),
            Some("Q1QvyIrUzz0"),
        );
        w.identity.register(&t);
        assert_eq!(w.identity.canonical("yt:Q1QvyIrUzz0"), "deezer:4274804272");
        assert_eq!(
            w.identity.canonical("deezer:4274804272"),
            "deezer:4274804272"
        );
    }

    #[test]
    fn same_song_found_under_other_catalog_and_title() {
        let w = world();
        let sc = ready(
            &w,
            "sc:2398309779",
            json!({"title": "Порезы на руках", "artists": ["Юпи"], "duration": 122}),
            Some("soundcloud"),
            Some("2398309779"),
        );
        w.identity.register(&sc);
        let found = w
            .identity
            .find_existing(&m(json!({"title": "Порезы на руках (feat. X)", "artists": ["Юпи"], "duration": 124})), None, true)
            .unwrap();
        assert_eq!(found.id, "sc:2398309779");
        assert!(
            w.identity
                .find_existing(
                    &m(json!({"title": "Порезы на руках", "artists": ["Юпи"], "duration": 90})),
                    None,
                    true
                )
                .is_none()
        );
    }

    #[test]
    fn isrc_match_wins_over_titles() {
        let w = world();
        let t = ready(
            &w,
            "deezer:1",
            json!({"title": "Song", "artists": ["A"], "duration": 200, "isrc": "US1"}),
            None,
            None,
        );
        w.identity.register(&t);
        let hit = w.identity.find_existing(
            &m(json!({"title": "Totally Different", "artists": ["B"], "isrc": "us1"})),
            None,
            true,
        );
        assert_eq!(hit.unwrap().id, "deezer:1");
    }

    #[test]
    fn link_follows_chains_and_refuses_cycles() {
        let w = world();
        assert!(w.identity.link("yt:AAAAAAAAAAA", "deezer:1", "manual"));
        assert!(w.identity.link("deezer:2", "yt:AAAAAAAAAAA", "reconciler"));
        assert_eq!(w.identity.canonical("deezer:2"), "deezer:1");
        assert!(!w.identity.link("deezer:1", "deezer:2", "cycle"));
        let many = w
            .identity
            .canonical_many(&["deezer:2".into(), "deezer:9".into()]);
        assert_eq!(many["deezer:2"], "deezer:1");
        assert_eq!(many["deezer:9"], "deezer:9");
    }

    #[test]
    fn not_ready_tracks_are_not_reused() {
        let w = world();
        w.store.remember_track(
            "deezer:1",
            &m(json!({"title": "Song", "artists": ["A"], "duration": 200})),
        );
        w.store.add_recording("a|song", "deezer:1", 200.0);
        let meta = m(json!({"title": "Song", "artists": ["A"], "duration": 200}));
        assert!(w.identity.find_existing(&meta, None, true).is_none());
        assert_eq!(
            w.identity.find_existing(&meta, None, false).unwrap().id,
            "deezer:1"
        );
        assert!(
            w.identity
                .find_existing(&meta, Some("deezer:1"), false)
                .is_none()
        );
    }
}

//! Likes (star/unstar), ratings and the starred lists.
//!
//! Navidrome is the owner of likes. Liking an external track downloads it with
//! priority and keeps a per-user intent; once the file is indexed the like is set in
//! Navidrome itself and the intent disappears. Until then the track shows up in that
//! user's starred list as a virtual song, so the like is visible immediately.

use super::browse::ensure_track;
use super::{Call, Reply};
use crate::app::Services;
use crate::ids::{self, Kind};
use crate::navidrome::NdError;
use crate::scheduler::{BACKGROUND, LIKE};
use crate::subsonic::models;
use crate::util::{Json, as_str, obj};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

const PARAMS: [&str; 3] = ["id", "albumId", "artistId"];

fn local(s: &Services, value: &str) -> Option<String> {
    s.store.get_track(&s.identity.canonical(value)).filter(|t| t.is_ready()).and_then(|t| t.local_id)
}

pub async fn star(s: &Arc<Services>, call: &Call) -> Reply {
    let values: Vec<(&str, Vec<String>)> = PARAMS.iter().map(|k| (*k, call.params.getall(k))).collect();
    if !values.iter().flat_map(|(_, v)| v).any(|v| ids::is_virtual(v)) {
        return Ok(None);
    }
    let starring = call.method == "star";
    let user = call.username();
    let mut forward: Vec<(&str, Vec<String>)> = PARAMS.iter().map(|k| (*k, Vec::new())).collect();
    let mut push = |key: &str, value: String| {
        if let Some((_, list)) = forward.iter_mut().find(|(k, _)| *k == key) {
            list.push(value);
        }
    };
    for (key, items) in &values {
        for value in items {
            let Some(vid) = ids::parse(value) else {
                push(key, value.clone());
                continue;
            };
            if vid.is_track() {
                if let Some(local_id) = local(s, value) {
                    s.store.remove_pending_star(user, value);
                    s.catalog.forget_song(user, &local_id);
                    push("id", local_id);
                } else if starring {
                    like_track(s, call, value).await;
                } else {
                    s.store.remove_pending_star(user, value);
                }
            } else if vid.kind == Kind::Album {
                if starring {
                    like_album(s, call, &vid.key).await;
                } else {
                    s.store.remove_pending_star(user, value);
                    if let Some(album) = s.intents.local_album(&vid.key, &call.creds()).await? {
                        push("albumId", album);
                    }
                }
            } else if starring {
                s.store.add_pending_star(user, value);
            } else {
                s.store.remove_pending_star(user, value);
            }
        }
    }
    let forward: Vec<(&str, Vec<String>)> = forward.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if !forward.is_empty() {
        match s.navidrome.call_many(&call.method, &call.params, &forward, &[]).await {
            Ok(_) => {}
            Err(NdError::Upstream { code, message }) => return Ok(Some(call.error(s, code, &message))),
            Err(e) => return Err(e),
        }
    }
    if starring {
        s.intents.maybe_flush(user, true);
    }
    Ok(Some(call.ok(s, None)))
}

async fn like_track(s: &Services, call: &Call, value: &str) {
    let track = ensure_track(s, value).await;
    s.store.add_pending_star(call.username(), value);
    if let Some(t) = track {
        s.scheduler.download(&t.id, LIKE, &format!("like:{}", call.username()), t.status == "failed");
    }
}

/// Liking an external album downloads all its tracks in the background.
async fn like_album(s: &Services, call: &Call, album_id: &str) {
    s.store.add_pending_star(call.username(), &ids::album(album_id));
    let (data, rows) = match tokio::try_join!(s.deezer.album(album_id), s.deezer.album_tracks(album_id)) {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!("album {album_id} unavailable for like: {e}");
            return;
        }
    };
    for track_id in s.catalog.remember_deezer(&rows, Some(&data)) {
        if let Some(t) = s.store.get_track(&s.identity.canonical(&track_id)).filter(|t| !t.is_ready()) {
            s.scheduler.download(&t.id, BACKGROUND, &format!("like-album:{}", call.username()), false);
        }
    }
}

pub async fn set_rating(s: &Arc<Services>, call: &Call) -> Reply {
    let value = call.params.get("id");
    if !ids::is_virtual(value) {
        return Ok(None);
    }
    // Not downloaded yet: accepted and dropped (Navidrome cannot rate what it does not have).
    let Some(local_id) = local(s, value) else { return Ok(Some(call.ok(s, None))) };
    let rating = call.params.get_or("rating", "0");
    match s.navidrome.call("setRating", &call.params, &[("id", &local_id), ("rating", rating)]).await {
        Ok(_) => Ok(Some(call.ok(s, None))),
        Err(NdError::Upstream { code, message }) => Ok(Some(call.error(s, code, &message))),
        Err(e) => Err(e),
    }
}

pub async fn get_starred(s: &Arc<Services>, call: &Call) -> Reply {
    let pending = s.store.pending_stars(Some(call.username()));
    if pending.is_empty() || !call.is_json() {
        return Ok(None);
    }
    let key = if call.method == "getStarred2" { "starred2" } else { "starred" };
    let folder = call.params.get("musicFolderId");
    let extra: Vec<(&str, &str)> = if call.params.contains("musicFolderId") { vec![("musicFolderId", folder)] } else { vec![] };
    let root = s.navidrome.call(&call.method, &call.params, &extra).await?;
    let starred = obj(root.get(key));
    let list = |k: &str| -> Vec<Value> { starred.get(k).and_then(Value::as_array).cloned().unwrap_or_default() };
    let (mut songs, mut albums, mut artists) = (list("song"), list("album"), list("artist"));
    let local_song_ids: HashSet<String> = songs.iter().map(|x| as_str(x.get("id"))).collect();
    for (_, target, created) in pending {
        let Some(vid) = ids::parse(&target) else { continue };
        if vid.is_track() {
            let Some(track) = s.store.get_track(&s.identity.canonical(&target)).or_else(|| s.store.get_track(&target)) else { continue };
            if track.local_id.as_ref().is_some_and(|l| local_song_ids.contains(l)) {
                continue;
            }
            songs.push(Value::Object(s.catalog.virtual_song(&target, &track, Some(created))));
        } else if vid.kind == Kind::Album {
            if let Some(data) = s.deezer.cached_album(&vid.key) {
                albums.push(Value::Object(models::album(&target, &data, None, Some(created))));
            }
        } else if let Ok(data) = s.deezer.artist(&vid.key).await {
            artists.push(Value::Object(models::artist(&target, &data, None, Some(created))));
        }
    }
    let mut result = Json::new();
    for (k, v) in [("artist", artists), ("album", albums), ("song", songs)] {
        if !v.is_empty() {
            result.insert(k.into(), Value::Array(v));
        }
    }
    Ok(Some(call.ok_with(s, key, Value::Object(result))))
}

//! Playlists with external tracks.
//!
//! Navidrome playlists can only hold library songs. Adding an external track queues
//! its download (like a like does) and keeps a per-user intent; the song is added
//! to the real playlist as soon as it is indexed. Meanwhile `getPlaylist` shows it
//! at the end of the playlist so the addition is visible right away.

use super::browse::ensure_track;
use super::{Call, Reply};
use crate::app::Services;
use crate::ids;
use crate::navidrome::NdError;
use crate::scheduler::LIKE;
use crate::util::{as_str, as_uint, obj};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

fn split(s: &Services, values: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut local, mut pending) = (Vec::new(), Vec::new());
    for value in values {
        if !ids::is_virtual(value) {
            local.push(value.clone());
            continue;
        }
        match s
            .store
            .get_track(&s.identity.canonical(value))
            .filter(|t| t.is_ready())
            .and_then(|t| t.local_id)
        {
            Some(l) => local.push(l),
            None => pending.push(value.clone()),
        }
    }
    (local, pending)
}

async fn queue(s: &Services, call: &Call, playlist_id: &str, pending: &[String]) {
    for value in pending {
        let Some(track) = ensure_track(s, value).await else {
            continue;
        };
        s.store
            .add_pending_playlist_add(call.username(), playlist_id, value);
        s.scheduler.download(
            &track.id,
            LIKE,
            &format!("playlist:{}", call.username()),
            track.status == "failed",
        );
    }
}

pub async fn create_playlist(s: &Arc<Services>, call: &Call) -> Reply {
    let values = call.params.getall("songId");
    if !values.iter().any(|v| ids::is_virtual(v)) {
        return Ok(None);
    }
    let (local, pending) = split(s, &values);
    let extra: Vec<(&str, &str)> = ["name", "playlistId"]
        .into_iter()
        .filter(|k| call.params.contains(k))
        .map(|k| (k, call.params.get(k)))
        .collect();
    let root = match s
        .navidrome
        .call_many("createPlaylist", &call.params, &[("songId", local)], &extra)
        .await
    {
        Ok(root) => root,
        Err(NdError::Upstream { code, message }) => return Ok(Some(call.error(s, code, &message))),
        Err(e) => return Err(e),
    };
    let playlist = obj(root.get("playlist"));
    let playlist_id = Some(as_str(playlist.get("id")))
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| call.params.get("playlistId").to_string());
    if !playlist_id.is_empty() {
        queue(s, call, &playlist_id, &pending).await;
    }
    Ok(Some(if playlist.is_empty() {
        call.ok(s, None)
    } else {
        call.ok_with(s, "playlist", Value::Object(playlist))
    }))
}

pub async fn update_playlist(s: &Arc<Services>, call: &Call) -> Reply {
    let values = call.params.getall("songIdToAdd");
    if !values.iter().any(|v| ids::is_virtual(v)) {
        return Ok(None);
    }
    let (local, pending) = split(s, &values);
    queue(s, call, call.params.get("playlistId"), &pending).await;
    Ok(Some(
        call.relay(
            s,
            "/rest/updatePlaylist",
            &call.params.replace("songIdToAdd", local),
        )
        .await?,
    ))
}

pub async fn get_playlist(s: &Arc<Services>, call: &Call) -> Reply {
    let playlist_id = call.params.get("id");
    let pending: Vec<String> = s
        .store
        .pending_playlist_adds(call.username())
        .into_iter()
        .filter(|(p, _)| p == playlist_id)
        .map(|(_, t)| t)
        .collect();
    if pending.is_empty() || !call.is_json() {
        return Ok(None);
    }
    let root = s
        .navidrome
        .call("getPlaylist", &call.params, &[("id", playlist_id)])
        .await?;
    let mut playlist = obj(root.get("playlist"));
    let mut entries: Vec<Value> = playlist
        .get("entry")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let present: HashSet<String> = entries.iter().map(|e| as_str(e.get("id"))).collect();
    let extra = s
        .catalog
        .present(&pending, &call.creds(), &HashSet::new())
        .await;
    entries.extend(
        extra
            .into_iter()
            .filter(|x| !present.contains(&as_str(x.get("id"))))
            .map(Value::Object),
    );
    let duration: i64 = entries.iter().map(|e| as_uint(e.get("duration"))).sum();
    playlist.insert("songCount".into(), json!(entries.len()));
    playlist.insert("duration".into(), json!(duration));
    playlist.insert("entry".into(), Value::Array(entries));
    Ok(Some(call.ok_with(s, "playlist", Value::Object(playlist))))
}

//! Playing external tracks, and translating ids in playback bookkeeping calls.

use super::browse::ensure_track;
use super::{Call, Reply};
use crate::app::Services;
use crate::db::Track;
use crate::ids;
use crate::scheduler::{PLAY, PREFETCH};
use crate::subsonic::models;
use crate::subsonic::response as sr;
use crate::util::{as_str, as_uint};
use axum::http::{HeaderValue, Method, Request, header};
use axum::response::{IntoResponse, Response};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

fn wants_transcoding(call: &Call, track: &Track) -> bool {
    let fmt = call.params.get("format").to_lowercase();
    if !fmt.is_empty() && fmt != "raw" && fmt != as_str(track.audio.get("suffix")) {
        return true;
    }
    let max_rate = call.params.int("maxBitRate", 0);
    0 < max_rate && max_rate < as_uint(track.audio.get("bitrate"))
}

/// Queue the next tracks of the album so album playback never stalls.
fn prefetch_next(s: &Arc<Services>, track: &Track) {
    let count = s.settings.prefetch_next_tracks;
    let Some(album_id) = track.album_id.clone().filter(|_| count > 0) else { return };
    let (me, id) = (s.clone(), track.id.clone());
    s.background.spawn(async move {
        let rows = match me.deezer.cached_album_tracks(&album_id) {
            Some(rows) => rows,
            None => match me.deezer.album_tracks(&album_id).await {
                Ok(rows) => rows,
                Err(_) => return,
            },
        };
        let track_ids = me.catalog.remember_deezer(&rows, None);
        let Some(position) = track_ids.iter().position(|t| *t == id) else { return };
        for next in track_ids.iter().skip(position + 1).take(count) {
            if let Some(t) = me.store.get_track(&me.identity.canonical(next)).filter(|t| !t.is_ready()) {
                me.scheduler.download(&t.id, PREFETCH, "prefetch", false);
            }
        }
    });
}

async fn serve_file(s: &Services, call: &Call, track: &Track) -> Response {
    let path = std::path::PathBuf::from(track.file.clone().unwrap_or_default());
    let inside = match (s.settings.music_dir.canonicalize(), path.canonicalize()) {
        (Ok(root), Ok(real)) if real.starts_with(&root) && real != root && real.is_file() => Some(real),
        _ => None,
    };
    let Some(real) = inside else { return call.error(s, sr::NOT_FOUND, "File not found") };
    // ServeFile answers Range/If-Range/HEAD; the request carries the client's headers.
    let method = if call.http_method == Method::HEAD { Method::HEAD } else { Method::GET };
    let mut req = Request::builder().method(method).uri("/");
    for (k, v) in &call.headers {
        req = req.header(k, v);
    }
    let Ok(req) = req.body(axum::body::Body::empty()) else { return call.error(s, sr::GENERIC, "bad request") };
    let mut response = match tower_http::services::ServeFile::new(&real).oneshot(req).await {
        Ok(r) => r.into_response(),
        Err(e) => return call.error(s, sr::NOT_FOUND, &format!("File not found: {e}")),
    };
    let suffix = crate::audio::suffix_of(&real);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(models::content_type(&suffix)));
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    if call.method == "download" {
        let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("track");
        let value = format!("attachment; filename*=UTF-8''{}", utf8_percent_encode(name, NON_ALPHANUMERIC));
        if let Ok(v) = HeaderValue::from_str(&value) {
            headers.insert(header::CONTENT_DISPOSITION, v);
        }
    }
    response
}

pub async fn stream(s: &Arc<Services>, call: &Call) -> Reply {
    let requested = call.params.get("id");
    if !ids::parse(requested).is_some_and(|v| v.is_track()) {
        return Ok(None);
    }
    let Some(mut track) = ensure_track(s, requested).await else { return Ok(Some(call.not_found(s, "Song"))) };
    if !track.is_ready() {
        let started = Instant::now();
        // An explicit play always runs now, even if a retry was scheduled for later.
        s.scheduler.download(&track.id, PLAY, "play", true);
        prefetch_next(s, &track);
        match s.scheduler.wait(&track.id, Duration::from_secs_f64(s.settings.stream_wait_seconds.max(0.0))).await {
            Some(ready) => {
                tracing::info!("first play of {requested} ready after {:.1}s", started.elapsed().as_secs_f64());
                track = ready;
            }
            None => {
                let reason = s.store.get_track(&track.id).and_then(|t| t.error).filter(|e| !e.is_empty());
                let reason = reason.unwrap_or_else(|| "download did not finish in time".into());
                return Ok(Some(call.error(s, sr::NOT_FOUND, &format!("Track is not available: {reason}"))));
            }
        }
    } else {
        prefetch_next(s, &track);
    }
    if let Some(local_id) = track.local_id.clone().filter(|_| wants_transcoding(call, &track)) {
        return Ok(Some(call.relay(s, "/rest/stream", &call.params.replace("id", [local_id])).await?));
    }
    Ok(Some(serve_file(s, call, &track).await))
}

/// Map ids to Navidrome ids: (translated, external ids still unmapped).
fn translate(s: &Services, values: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut translated, mut pending) = (Vec::new(), Vec::new());
    for value in values {
        if ids::parse(value).is_none() {
            translated.push(value.clone());
            continue;
        }
        match s.store.get_track(&s.identity.canonical(value)).filter(|t| t.is_ready()).and_then(|t| t.local_id) {
            Some(local) => translated.push(local),
            None => pending.push(value.clone()),
        }
    }
    (translated, pending)
}

pub async fn scrobble(s: &Arc<Services>, call: &Call) -> Reply {
    let values = call.params.getall("id");
    if !values.iter().any(|v| ids::is_virtual(v)) {
        return Ok(None);
    }
    let times = call.params.getall("time");
    let submission = call.params.get_or("submission", "true").to_lowercase() != "false";
    let now_ms = (crate::util::now() * 1000.0) as i64;
    let (mut local_ids, mut local_times) = (Vec::new(), Vec::new());
    for (index, value) in values.iter().enumerate() {
        let when = times.get(index).and_then(|t| t.parse::<i64>().ok()).filter(|t| *t >= 0).unwrap_or(now_ms);
        let (translated, pending) = translate(s, std::slice::from_ref(value));
        if let Some(local) = translated.into_iter().next() {
            local_ids.push(local);
            local_times.push(when.to_string());
        } else if !pending.is_empty() && submission {
            s.store.add_pending_scrobble(call.username(), value, when, true);
        }
    }
    if !local_ids.is_empty() {
        let mut repeated = vec![("id", local_ids)];
        if !times.is_empty() {
            repeated.push(("time", local_times));
        }
        let flag = if submission { "true" } else { "false" };
        s.navidrome.call_many("scrobble", &call.params, &repeated, &[("submission", flag)]).await?;
    }
    Ok(Some(call.ok(s, None)))
}

pub async fn save_play_queue(s: &Arc<Services>, call: &Call) -> Reply {
    let values = call.params.getall("id");
    let current = call.params.get("current").to_string();
    if !values.iter().chain([&current]).any(|v| ids::is_virtual(v)) {
        return Ok(None);
    }
    let (translated, _) = translate(s, &values);
    if translated.is_empty() {
        return Ok(Some(call.ok(s, None)));
    }
    let current_ids = if current.is_empty() { vec![] } else { translate(s, &[current]).0 };
    let mut params = call.params.replace("id", translated).replace("current", current_ids.iter().take(1).cloned());
    if current_ids.is_empty() {
        params = params.replace("position", Vec::<String>::new());
    }
    Ok(Some(call.relay(s, "/rest/savePlayQueue", &params).await?))
}

pub async fn save_play_queue_by_index(s: &Arc<Services>, call: &Call) -> Reply {
    let values = call.params.getall("id");
    if !values.iter().any(|v| ids::is_virtual(v)) {
        return Ok(None);
    }
    let index = call.params.int("currentIndex", 0);
    let (mut kept, mut new_index) = (Vec::new(), 0);
    for (position, value) in values.iter().enumerate() {
        if let Some(local) = translate(s, std::slice::from_ref(value)).0.into_iter().next() {
            if position as i64 <= index {
                new_index = kept.len();
            }
            kept.push(local);
        }
    }
    if kept.is_empty() {
        return Ok(Some(call.ok(s, None)));
    }
    let params = call.params.replace("id", kept).with("currentIndex", new_index.to_string());
    Ok(Some(call.relay(s, "/rest/savePlayQueueByIndex", &params).await?))
}

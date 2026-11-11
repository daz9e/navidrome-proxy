//! Browsing external entities, and completing local albums/artists with them.

use super::{Call, Reply};
use crate::app::Services;
use crate::db::Track;
use crate::deezer::{self, DeezerError};
use crate::ids::{self, Kind};
use crate::matching::{self, Credits};
use crate::navidrome::NdError;
use crate::subsonic::models;
use crate::subsonic::params::Params;
use crate::util::{Json, as_str, as_uint, obj, opt_str, truthy};
use axum::http::header;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

const DAY: f64 = 86400.0;
/// At most two background album lookups at a time (library syncs open every album).
static LOOKUPS: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| tokio::sync::Semaphore::new(2));

fn values(items: Vec<Json>) -> Vec<Value> {
    items.into_iter().map(Value::Object).collect()
}

/// The stored track for an external id, fetching unknown Deezer ids on demand.
pub async fn ensure_track(s: &Services, track_id: &str) -> Option<Track> {
    if let Some(t) = s.store.get_track(&s.identity.canonical(track_id)).or_else(|| s.store.get_track(track_id)) {
        return Some(t);
    }
    let vid = ids::parse(track_id).filter(|v| v.kind == Kind::Track)?;
    let row = s.deezer.track(&vid.key).await.ok()?;
    s.catalog.remember_deezer(&[row], None);
    s.store.get_track(track_id)
}

// ------------------------------------------------------------------- songs
pub async fn get_song(s: &Arc<Services>, call: &Call) -> Reply {
    let song_id = call.params.get("id");
    if !ids::parse(song_id).is_some_and(|v| v.is_track()) {
        return Ok(None);
    }
    let Some(track) = ensure_track(s, song_id).await else { return Ok(Some(call.not_found(s, "Song"))) };
    let starred = s.catalog.starred_map(call.username()).get(song_id).copied();
    Ok(Some(call.ok_with(s, "song", Value::Object(s.catalog.virtual_song(song_id, &track, starred)))))
}

// ------------------------------------------------------------------ albums
/// The Navidrome album that already holds tracks of a Deezer album, if any.
pub async fn local_album_id(s: &Services, deezer_album_id: &str, creds: &Params) -> Result<Option<String>, NdError> {
    let cache_key = format!("local-album:{deezer_album_id}");
    if let Some(cached) = s.store.cache_get(&cache_key).and_then(|v| opt_str(Some(&v))).filter(|v| !v.is_empty()) {
        return Ok(Some(cached));
    }
    let has_local = s.store.tracks_in_album(deezer_album_id).iter().any(|x| {
        s.store.get_track(&s.identity.canonical(&x.id)).is_some_and(|t| t.local_id.is_some() && t.is_ready())
    });
    if !has_local {
        return Ok(None);
    }
    let album_id = s.intents.local_album(deezer_album_id, creds).await?;
    if let Some(id) = &album_id {
        s.store.cache_set(&cache_key, &json!(id), 7.0 * DAY);
        s.store.cache_set(&format!("deezer-album-for:{id}"), &json!(deezer_album_id), 30.0 * DAY);
    }
    Ok(album_id)
}

pub async fn local_album_object(s: &Services, local_id: &str, creds: &Params) -> Result<Option<Json>, NdError> {
    match s.navidrome.call("getAlbum", creds, &[("id", local_id)]).await {
        Ok(root) => Ok(root.get("album").and_then(Value::as_object).cloned()),
        Err(NdError::Upstream { .. }) => Ok(None),
        Err(e) => Err(e),
    }
}

fn listing(mut album: Json) -> Json {
    album.remove("song");
    album
}

/// AlbumID3 entries for Deezer album rows, local albums replacing their catalog twin.
pub async fn merge_with_local(s: &Services, call: &Call, rows: Vec<Json>) -> Result<Vec<Json>, NdError> {
    let creds = call.creds();
    let one = |row: Json| {
        let creds = &creds;
        async move {
            let key = as_str(row.get("id"));
            if let Some(local_id) = local_album_id(s, &key, creds).await?
                && let Some(local) = local_album_object(s, &local_id, creds).await?
            {
                return Ok(listing(local));
            }
            Ok(models::album(&ids::album(&key), &row, None, None))
        }
    };
    futures::future::try_join_all(rows.into_iter().map(one)).await
}

/// One entry per album title: the edition with most tracks, explicit preferred.
pub fn dedupe_editions(albums: Vec<Json>) -> Vec<Json> {
    let rank = |a: &Json| (as_uint(a.get("nb_tracks")), truthy(a.get("explicit_lyrics")));
    let mut best: Vec<(String, Json)> = Vec::new();
    for album in albums {
        let key = matching::normalize(&as_str(album.get("title")));
        match best.iter_mut().find(|(k, _)| *k == key) {
            Some((_, current)) => {
                if rank(&album) > rank(current) {
                    *current = album;
                }
            }
            None => best.push((key, album)),
        }
    }
    let mut out: Vec<Json> = best.into_iter().map(|(_, a)| a).collect();
    out.sort_by_key(|a| std::cmp::Reverse(as_str(a.get("release_date"))));
    out
}

async fn virtual_album(s: &Services, call: &Call, album_id: &str) -> Option<Json> {
    let (data, rows) = tokio::try_join!(s.deezer.album(album_id), s.deezer.album_tracks(album_id)).ok()?;
    let track_ids = s.catalog.remember_deezer(&rows, Some(&data));
    let songs = s.catalog.present(&track_ids, &call.creds(), &HashSet::new()).await;
    let starred = s.catalog.starred_map(call.username()).get(&ids::album(album_id)).copied();
    Some(models::album(&ids::album(album_id), &data, Some(values(songs)), starred))
}

pub async fn get_album(s: &Arc<Services>, call: &Call) -> Reply {
    let album_id = call.params.get("id");
    if let Some(vid) = ids::parse(album_id) {
        if vid.kind != Kind::Album {
            return Ok(Some(call.not_found(s, "Album")));
        }
        let creds = call.creds();
        if let Some(local_id) = local_album_id(s, &vid.key, &creds).await?
            && let Some(mut local) = local_album_object(s, &local_id, &creds).await?
        {
            complete_album(s, call, &mut local, Some(vid.key.clone()), true).await;
            local.insert("id".into(), json!(album_id)); // keep the id the client asked for
            return Ok(Some(call.ok_with(s, "album", Value::Object(local))));
        }
        return Ok(Some(match virtual_album(s, call, &vid.key).await {
            Some(album) => call.ok_with(s, "album", Value::Object(album)),
            None => call.not_found(s, "Album"),
        }));
    }
    if !(s.settings.complete_local_albums && call.is_json()) {
        return Ok(None);
    }
    let mut album = match s.navidrome.call("getAlbum", &call.params, &[("id", album_id)]).await {
        Ok(root) => obj(root.get("album")),
        Err(NdError::Upstream { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    complete_album(s, call, &mut album, None, false).await;
    // The album was fetched already: answer with it instead of asking Navidrome twice.
    Ok(Some(call.ok_with(s, "album", Value::Object(album))))
}

/// Append the catalog tracks a local album is missing, as virtual songs.
///
/// In the request path only cached catalog data is used; a cold album is fetched
/// in the background (rate limited) and completed on the next open. That keeps a
/// client's full library sync (one getAlbum per album) cheap. Albums that were in
/// the library before the proxy are associated with the catalog by name.
pub async fn complete_album(s: &Arc<Services>, call: &Call, album: &mut Json, deezer_album: Option<String>, fetch: bool) -> usize {
    let songs: Vec<Json> = album.get("song").and_then(Value::as_array).into_iter().flatten().filter_map(|x| x.as_object().cloned()).collect();
    let song_ids: Vec<String> = songs.iter().map(|x| as_str(x.get("id"))).collect();
    let deezer_album = match deezer_album {
        Some(d) => d,
        None => {
            let mapped = s.store.tracks_by_local_ids(song_ids.iter().map(String::as_str));
            let mut counts: Vec<(String, usize)> = Vec::new();
            for id in &song_ids {
                if let Some(a) = mapped.get(id).and_then(|t| t.album_id.clone()) {
                    match counts.iter_mut().find(|(k, _)| *k == a) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((a, 1)),
                    }
                }
            }
            // max_by_key keeps the last maximum; reversing makes ties go to the first seen.
            match counts.into_iter().rev().max_by_key(|(_, n)| *n) {
                Some((a, _)) => a,
                None => {
                    let local_id = as_str(album.get("id"));
                    match s.store.cache_get(&format!("deezer-album-for:{local_id}")) {
                        None => {
                            let (me, name, artist) = (s.clone(), as_str(album.get("name")), as_str(album.get("artist")));
                            s.background.spawn(async move { associate(&me, &local_id, &name, &artist).await });
                            return 0;
                        }
                        Some(v) => match opt_str(Some(&v)).filter(|v| !v.is_empty()) {
                            Some(d) => d,
                            None => return 0, // known to have no catalog twin
                        },
                    }
                }
            }
        }
    };
    let rows = match s.deezer.cached_album_tracks(&deezer_album) {
        Some(rows) => rows,
        None if !fetch => {
            let me = s.clone();
            s.background.spawn(async move { warm_album(&me, &deezer_album).await });
            return 0;
        }
        None => match async { s.deezer.album(&deezer_album).await?; s.deezer.album_tracks(&deezer_album).await }.await {
            Ok(rows) => rows,
            Err(_) => return 0,
        },
    };
    let data = s.deezer.cached_album(&deezer_album);
    let track_ids = s.catalog.remember_deezer(&rows, data.as_ref());
    let canonical = s.identity.canonical_many(&track_ids);
    let have_local: HashSet<String> = song_ids.iter().cloned().collect();
    let have_titles: HashSet<String> = songs.iter().map(|x| matching::base_title(&as_str(x.get("title")))).collect();
    let tracks = s.store.get_tracks(canonical.values().map(String::as_str));
    let missing: Vec<String> = track_ids
        .iter()
        .filter(|id| {
            let Some(t) = canonical.get(*id).and_then(|c| tracks.get(c)) else { return false };
            !(t.local_id.as_ref().is_some_and(|l| have_local.contains(l)) || have_titles.contains(&matching::base_title(&t.title())))
        })
        .cloned()
        .collect();
    if missing.is_empty() {
        return 0;
    }
    let extra = s.catalog.present(&missing, &call.creds(), &have_local).await;
    let added = extra.len();
    let mut merged: Vec<Json> = songs.into_iter().chain(extra).collect();
    merged.sort_by_key(|x| {
        let disc = match as_uint(x.get("discNumber")) {
            0 => 1,
            d => d,
        };
        (disc, as_uint(x.get("track")))
    });
    let duration: i64 = merged.iter().map(|x| as_uint(x.get("duration"))).sum();
    album.insert("songCount".into(), json!(merged.len()));
    album.insert("duration".into(), json!(duration));
    album.insert("song".into(), Value::Array(values(merged)));
    added
}

/// Find the catalog twin of a local album by exact title and artist (cached, also negatives).
async fn associate(s: &Services, local_album: &str, name: &str, artist: &str) {
    let key = format!("deezer-album-for:{local_album}");
    if name.is_empty() || s.store.cache_get(&key).is_some() {
        return;
    }
    let Ok(_permit) = LOOKUPS.acquire().await else { return };
    let Ok(rows) = s.deezer.search_albums(format!("{artist} {name}").trim(), 10).await else { return };
    let found = rows
        .iter()
        .find(|row| {
            matching::normalize(&as_str(row.get("title"))) == matching::normalize(name)
                && matching::artists_match(&[artist.to_string()], Credits::Text(&as_str(obj(row.get("artist")).get("name")))) == 1.0
        })
        .map(|row| as_str(row.get("id")))
        .unwrap_or_default();
    s.store.cache_set(&key, &json!(found), 30.0 * DAY);
    if !found.is_empty() {
        warm_album(s, &found).await;
    }
}

async fn warm_album(s: &Services, album_id: &str) {
    if let Err(e) = async { s.deezer.album(album_id).await?; s.deezer.album_tracks(album_id).await }.await {
        tracing::debug!("album {album_id} warm-up failed: {e}");
    }
}

// ----------------------------------------------------------------- artists
async fn virtual_artist(s: &Services, call: &Call, artist_id: &str) -> Result<Option<Json>, NdError> {
    let Ok((data, albums)) = tokio::try_join!(s.deezer.artist(artist_id), s.deezer.artist_albums(artist_id)) else { return Ok(None) };
    let rows: Vec<Json> = dedupe_editions(albums)
        .into_iter()
        .map(|mut a| {
            if !truthy(a.get("artist")) {
                a.insert("artist".into(), Value::Object(data.clone()));
            }
            a
        })
        .collect();
    let listed = merge_with_local(s, call, rows).await?;
    let starred = s.catalog.starred_map(call.username()).get(&ids::artist(artist_id)).copied();
    Ok(Some(models::artist(&ids::artist(artist_id), &data, Some(values(listed)), starred)))
}

pub async fn get_artist(s: &Arc<Services>, call: &Call) -> Reply {
    let artist_id = call.params.get("id");
    if let Some(vid) = ids::parse(artist_id) {
        if vid.kind != Kind::Artist {
            return Ok(Some(call.not_found(s, "Artist")));
        }
        return Ok(Some(match virtual_artist(s, call, &vid.key).await? {
            Some(artist) => call.ok_with(s, "artist", Value::Object(artist)),
            None => call.not_found(s, "Artist"),
        }));
    }
    if !(s.settings.complete_local_albums && call.is_json()) {
        return Ok(None);
    }
    let mut artist = match s.navidrome.call("getArtist", &call.params, &[("id", artist_id)]).await {
        Ok(root) => obj(root.get("artist")),
        Err(NdError::Upstream { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let _ = tokio::time::timeout(Duration::from_secs(3), complete_artist(s, &mut artist)).await;
    Ok(Some(call.ok_with(s, "artist", Value::Object(artist))))
}

async fn find_deezer_artist(s: &Services, name: &str) -> Result<Option<Json>, DeezerError> {
    let wanted = matching::normalize(name);
    if wanted.is_empty() {
        return Ok(None);
    }
    Ok(s.deezer.search_artists(name, 5).await?.into_iter().find(|row| matching::normalize(&as_str(row.get("name"))) == wanted))
}

/// Append catalog albums a local artist does not have yet.
async fn complete_artist(s: &Services, artist: &mut Json) -> Result<usize, DeezerError> {
    let Some(found) = find_deezer_artist(s, &as_str(artist.get("name"))).await? else { return Ok(0) };
    let local: Vec<Value> = artist.get("album").and_then(Value::as_array).into_iter().flatten().filter(|a| a.is_object()).cloned().collect();
    let have: HashSet<String> = local.iter().map(|a| matching::normalize(&as_str(a.get("name")))).collect();
    let mut extra = Vec::new();
    for mut row in dedupe_editions(s.deezer.artist_albums(&as_str(found.get("id"))).await?) {
        if have.contains(&matching::normalize(&as_str(row.get("title")))) {
            continue;
        }
        if !truthy(row.get("artist")) {
            row.insert("artist".into(), Value::Object(found.clone()));
        }
        extra.push(Value::Object(models::album(&ids::album(as_str(row.get("id"))), &row, None, None)));
    }
    if extra.is_empty() {
        return Ok(0);
    }
    let added = extra.len();
    let albums: Vec<Value> = local.into_iter().chain(extra).collect();
    artist.insert("albumCount".into(), json!(albums.len()));
    artist.insert("album".into(), Value::Array(albums));
    Ok(added)
}

fn images(item: &Json, kind: &str) -> Json {
    let md5 = as_str(item.get("md5_image"));
    let image_kind = if kind == "picture" { "artist" } else { "cover" };
    let pick = |size: u32, suffix: &str| deezer::cover_url(&md5, size, image_kind).unwrap_or_else(|| as_str(item.get(&format!("{kind}_{suffix}"))));
    let v = json!({
        "smallImageUrl": pick(250, "medium"),
        "mediumImageUrl": pick(500, "big"),
        "largeImageUrl": pick(1000, "xl"),
    });
    v.as_object().cloned().unwrap_or_default()
}

pub async fn get_artist_info(s: &Arc<Services>, call: &Call) -> Reply {
    let Some(vid) = ids::parse(call.params.get("id")) else { return Ok(None) };
    let key = if call.method == "getArtistInfo2" { "artistInfo2" } else { "artistInfo" };
    if vid.kind != Kind::Artist {
        return Ok(Some(call.ok_with(s, key, json!({}))));
    }
    let count = call.params.int("count", 20).max(0) as usize;
    let Ok((data, related)) = tokio::try_join!(s.deezer.artist(&vid.key), s.deezer.artist_related(&vid.key, count)) else {
        return Ok(Some(call.ok_with(s, key, json!({}))));
    };
    let mut info = Json::new();
    info.insert("biography".into(), json!(""));
    info.insert("musicBrainzId".into(), json!(""));
    info.insert("lastFmUrl".into(), json!(""));
    info.extend(images(&data, "picture"));
    let similar: Vec<Value> = related.iter().map(|a| Value::Object(models::artist(&ids::artist(as_str(a.get("id"))), a, None, None))).collect();
    info.insert("similarArtist".into(), Value::Array(similar));
    Ok(Some(call.ok_with(s, key, Value::Object(info))))
}

pub async fn get_album_info(s: &Arc<Services>, call: &Call) -> Reply {
    let Some(vid) = ids::parse(call.params.get("id")) else { return Ok(None) };
    let key = "albumInfo";
    if vid.kind != Kind::Album {
        return Ok(Some(call.ok_with(s, key, json!({}))));
    }
    let Ok(data) = s.deezer.album(&vid.key).await else { return Ok(Some(call.ok_with(s, key, json!({})))) };
    let mut info = Json::new();
    info.insert("notes".into(), json!(""));
    info.insert("musicBrainzId".into(), json!(""));
    info.insert("lastFmUrl".into(), json!(""));
    info.extend(images(&data, "cover"));
    Ok(Some(call.ok_with(s, key, Value::Object(info))))
}

pub async fn get_top_songs(s: &Arc<Services>, call: &Call) -> Reply {
    let name = call.params.get("artist");
    let count = call.params.int("count", 50).max(0);
    let local = match s.navidrome.call("getTopSongs", &call.params, &[("artist", name), ("count", &count.to_string())]).await {
        Ok(root) => Some(root),
        Err(NdError::Upstream { .. }) => None,
        Err(e) => return Err(e),
    };
    if let Some(root) = local.filter(|r| !crate::navidrome::songs_of(r.get("topSongs")).is_empty()) {
        // Already fetched: re-render JSON instead of asking Navidrome twice; XML clients get Navidrome's own bytes.
        return Ok(call.is_json().then(|| call.ok_with(s, "topSongs", root.get("topSongs").cloned().unwrap_or(json!({})))));
    }
    let rows = match find_deezer_artist(s, name).await {
        Ok(Some(found)) => s.deezer.artist_top(&as_str(found.get("id")), count.min(50) as usize).await.unwrap_or_default(),
        _ => vec![],
    };
    let track_ids = s.catalog.remember_deezer(&rows, None);
    let songs = s.catalog.present(&track_ids, &call.creds(), &HashSet::new()).await;
    let body = if songs.is_empty() { json!({}) } else { json!({"song": songs}) };
    Ok(Some(call.ok_with(s, "topSongs", body)))
}

pub async fn get_similar_songs(s: &Arc<Services>, call: &Call) -> Reply {
    let Some(vid) = ids::parse(call.params.get("id")) else { return Ok(None) };
    let key = if call.method == "getSimilarSongs2" { "similarSongs2" } else { "similarSongs" };
    let mut rows = Vec::new();
    if vid.kind == Kind::Artist {
        let result: Result<(), DeezerError> = async {
            rows = s.deezer.artist_top(&vid.key, 10).await?;
            for related in s.deezer.artist_related(&vid.key, 4).await?.iter().take(4) {
                rows.extend(s.deezer.artist_top(&as_str(related.get("id")), 5).await?);
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            rows.clear();
        }
    }
    let track_ids = s.catalog.remember_deezer(&rows, None);
    let songs = s.catalog.present(&track_ids, &call.creds(), &HashSet::new()).await;
    let body = if songs.is_empty() { json!({}) } else { json!({"song": songs}) };
    Ok(Some(call.ok_with(s, key, body)))
}

// ------------------------------------------------------------------- media
/// The CDN variant of a cover URL closest to the requested size.
pub fn sized(url: &str, size: i64) -> String {
    for known in [1000, 500, 250] {
        let token = format!("/{known}x{known}-");
        if url.contains(&token) {
            let target = if size > 0 && size <= 250 {
                250
            } else if size > 0 && size <= 500 {
                500
            } else {
                1000
            };
            return url.replace(&token, &format!("/{target}x{target}-"));
        }
    }
    url.to_string()
}

pub async fn get_cover_art(s: &Arc<Services>, call: &Call) -> Reply {
    let art_id = call.params.get("id");
    let Some(vid) = ids::parse(art_id) else { return Ok(None) };
    let size = call.params.int("size", 0);
    let url = if vid.is_track() {
        let track = ensure_track(s, art_id).await;
        let url = track.and_then(|t| opt_str(t.meta.get("cover"))).filter(|u| !u.is_empty());
        match (url, vid.kind) {
            (None, Kind::Youtube) => Some(format!("https://i.ytimg.com/vi/{}/hqdefault.jpg", vid.key)),
            (url, _) => url,
        }
    } else if vid.kind == Kind::Album {
        s.deezer.album(&vid.key).await.ok().and_then(|a| deezer::best_cover(&a))
    } else {
        s.deezer
            .artist(&vid.key)
            .await
            .ok()
            .and_then(|a| deezer::cover_url(&as_str(a.get("md5_image")), 1000, "artist").or_else(|| opt_str(a.get("picture_xl"))))
    };
    let found = match url {
        Some(url) => s.covers.get(&sized(&url, size)).await,
        None => None,
    };
    Ok(Some(match found {
        Some((data, ctype)) => ([(header::CONTENT_TYPE, ctype), (header::CACHE_CONTROL, "public, max-age=604800")], data).into_response(),
        None => (axum::http::StatusCode::NOT_FOUND, "cover not found").into_response(),
    }))
}

pub async fn get_lyrics_by_song_id(s: &Arc<Services>, call: &Call) -> Reply {
    let id = call.params.get("id");
    if ids::parse(id).is_none() {
        return Ok(None);
    }
    if let Some(track) = s.store.get_track(&s.identity.canonical(id)).filter(|t| t.is_ready())
        && let Some(local_id) = track.local_id
    {
        match s.navidrome.call("getLyricsBySongId", &call.params, &[("id", &local_id)]).await {
            Ok(root) => return Ok(Some(call.ok_with(s, "lyricsList", root.get("lyricsList").cloned().unwrap_or(json!({}))))),
            Err(NdError::Upstream { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(call.ok_with(s, "lyricsList", json!({}))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sized_picks_the_cdn_variant() {
        let url = "https://cdn/images/cover/x/1000x1000-000000-80-0-0.jpg";
        assert_eq!(sized(url, 200), "https://cdn/images/cover/x/250x250-000000-80-0-0.jpg");
        assert_eq!(sized(url, 300), "https://cdn/images/cover/x/500x500-000000-80-0-0.jpg");
        assert_eq!(sized(url, 0), url);
        assert_eq!(sized("https://i.ytimg.com/vi/x/hqdefault.jpg", 100), "https://i.ytimg.com/vi/x/hqdefault.jpg");
    }

    #[test]
    fn dedupe_keeps_the_biggest_explicit_edition_newest_first() {
        let a = |id: u64, title: &str, n: u64, explicit: bool, date: &str| {
            json!({"id": id, "title": title, "nb_tracks": n, "explicit_lyrics": explicit, "release_date": date}).as_object().cloned().unwrap()
        };
        let out = dedupe_editions(vec![a(1, "RAW", 10, false, "2020"), a(2, "raw", 10, true, "2020"), a(3, "Old", 5, false, "2010"), a(4, "RAW", 3, true, "2021")]);
        let ids: Vec<i64> = out.iter().map(|x| x["id"].as_i64().unwrap()).collect();
        assert_eq!(ids, [2, 3]);
    }
}

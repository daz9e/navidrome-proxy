//! Virtual Subsonic entities shaped exactly like Navidrome 0.63 objects.
//!
//! Strict clients decode responses with typed models (Arpeggi and Resonus use Swift
//! `Codable`): a missing key, a `null` or a boolean where a date string belongs
//! crashes them. Every builder here therefore emits the same key set and JSON types
//! as the real Navidrome object, captured from a live server.

use crate::ids;
use crate::util::{Json, arr, as_f64, as_str, as_uint, obj, opt_str, truthy};
use serde_json::{Value, json};
use time::macros::format_description;
use time::{Date, OffsetDateTime, PrimitiveDateTime, UtcOffset};

pub fn content_type(suffix: &str) -> &'static str {
    match suffix {
        "m4a" | "mp4" => "audio/mp4",
        "mp3" => "audio/mpeg",
        "opus" | "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        _ => "audio/mp4",
    }
}

fn format_utc(dt: OffsetDateTime) -> String {
    let dt = dt.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    )
}

fn parse_iso(text: &str) -> Option<OffsetDateTime> {
    let text = text.trim().replace('Z', "+00:00").replacen(' ', "T", 1);
    if let Ok(dt) = OffsetDateTime::parse(&text, &time::format_description::well_known::Iso8601::DEFAULT) {
        return Some(dt);
    }
    let naive = text.split('.').next().unwrap_or(&text);
    if let Ok(dt) = PrimitiveDateTime::parse(naive, format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]")) {
        return Some(dt.assume_utc());
    }
    Date::parse(&text, format_description!("[year]-[month]-[day]")).ok().map(|d| d.midnight().assume_utc())
}

/// Navidrome's timestamp format from a unix time; never empty.
pub fn iso_ts(ts: f64) -> String {
    let dt = OffsetDateTime::from_unix_timestamp_nanos((ts * 1e9) as i128).unwrap_or_else(|_| OffsetDateTime::now_utc());
    format_utc(dt)
}

/// Navidrome's timestamp format from a JSON value (unix time or ISO string); now when absent.
pub fn iso(value: Option<&Value>) -> String {
    if let Some(Value::String(s)) = value
        && let Some(dt) = parse_iso(s)
    {
        return format_utc(dt);
    }
    match as_f64(value) {
        Some(ts) if ts.is_finite() => iso_ts(ts),
        _ => format_utc(OffsetDateTime::now_utc()),
    }
}

/// OpenSubsonic ItemDate: only the known parts, like Navidrome.
pub fn item_date(date: &str) -> Json {
    let mut out = Json::new();
    let parts = date.split('-').filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    for (key, part) in ["year", "month", "day"].into_iter().zip(parts) {
        if let Ok(n) = part.parse::<i64>()
            && n != 0
        {
            out.insert(key.into(), json!(n));
        }
    }
    out
}

fn artist_refs(names: &[String], artist_id: &str) -> Vec<Value> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| json!({"id": if i == 0 { artist_id } else { "" }, "name": name}))
        .collect()
}

/// A virtual song (`Child`) for `deezer:`, `yt:` and `sc:` tracks.
pub fn song(track_id: &str, meta: &Json, audio: &Json, created: Option<&Value>, starred: Option<f64>) -> Json {
    let album_id = opt_str(meta.get("album_id")).map(ids::album).unwrap_or_default();
    let artist_id = opt_str(meta.get("artist_id")).map(ids::artist).unwrap_or_default();
    let mut names: Vec<String> = match meta.get("artists") {
        Some(Value::Array(a)) if !a.is_empty() => a.iter().map(|v| as_str(Some(v))).collect(),
        _ => vec![as_str(meta.get("artist"))],
    };
    names.retain(|n| !n.is_empty());
    if names.is_empty() {
        names.push("Unknown Artist".into());
    }
    let display = opt_str(meta.get("artist")).unwrap_or_else(|| names.join(", "));
    let album_artist = opt_str(meta.get("album_artist")).unwrap_or_else(|| names[0].clone());
    let suffix = opt_str(audio.get("suffix")).unwrap_or_else(|| "m4a".into());
    let youtube = ids::parse(track_id).is_some_and(|v| v.kind == ids::Kind::Youtube);
    let cover = if youtube || album_id.is_empty() { track_id.to_string() } else { album_id.clone() };
    let duration = as_f64(audio.get("duration")).filter(|d| *d != 0.0).or_else(|| as_f64(meta.get("duration")));
    let title = as_str(meta.get("title"));
    let genre = as_str(meta.get("genre"));
    let isrc = opt_str(meta.get("isrc"));
    let disc = as_uint(meta.get("disc"));
    let channels = as_uint(audio.get("channels"));
    let rate = as_uint(audio.get("sample_rate"));
    let mut out = json!({
        "id": track_id,
        "parent": album_id,
        "isDir": false,
        "title": if title.is_empty() { track_id.to_string() } else { title.clone() },
        "album": as_str(meta.get("album")),
        "artist": display,
        "track": as_uint(meta.get("track")),
        "year": as_uint(meta.get("year")),
        "genre": genre,
        "coverArt": cover,
        "size": as_uint(audio.get("size")),
        "contentType": content_type(&suffix),
        "suffix": suffix,
        "duration": duration.unwrap_or(0.0).round().max(0.0) as i64,
        "bitRate": as_uint(audio.get("bitrate")),
        "path": "",
        "discNumber": if disc == 0 { 1 } else { disc },
        "created": iso(created),
        "albumId": album_id,
        "artistId": artist_id,
        "type": "music",
        "bpm": 0,
        "comment": "",
        "sortName": title.to_lowercase(),
        "mediaType": "song",
        "musicBrainzId": "",
        "isrc": isrc.map(|i| vec![i]).unwrap_or_default(),
        "genres": if genre.is_empty() { json!([]) } else { json!([{"name": genre}]) },
        "replayGain": {},
        "channelCount": if channels == 0 { 2 } else { channels },
        "samplingRate": if rate == 0 { 44100 } else { rate },
        "bitDepth": 0,
        "moods": [],
        "artists": artist_refs(&names, &artist_id),
        "displayArtist": display,
        "albumArtists": artist_refs(std::slice::from_ref(&album_artist), &artist_id),
        "displayAlbumArtist": album_artist,
        "contributors": [],
        "displayComposer": "",
        "explicitStatus": if truthy(meta.get("explicit")) { "explicit" } else { "" },
        "groupings": [],
        "works": [],
        "movements": [],
    });
    let map = out.as_object_mut().unwrap();
    if let Some(ts) = starred.filter(|t| *t != 0.0) {
        map.insert("starred".into(), json!(iso_ts(ts)));
    }
    std::mem::take(map)
}

/// A virtual album (`AlbumID3`) from a Deezer album object.
pub fn album(album_id: &str, data: &Json, songs: Option<Vec<Value>>, starred: Option<f64>) -> Json {
    let artist = obj(data.get("artist"));
    let artist_name = as_str(artist.get("name"));
    let artist_id = opt_str(artist.get("id")).map(ids::artist).unwrap_or_default();
    let name = as_str(data.get("title"));
    let release = as_str(data.get("release_date"));
    let genres: Vec<String> = arr(obj(data.get("genres")).get("data"))
        .iter()
        .filter_map(|g| opt_str(g.get("name")))
        .collect();
    let record_type = as_str(data.get("record_type"));
    let mut duration = as_uint(data.get("duration"));
    if duration == 0
        && let Some(songs) = &songs
    {
        duration = songs.iter().map(|s| as_uint(s.get("duration"))).sum();
    }
    let year: String = release.chars().take(4).collect();
    let mut out = json!({
        "id": album_id,
        "name": name,
        "artist": artist_name,
        "artistId": artist_id,
        "coverArt": album_id,
        "songCount": songs.as_ref().map(|s| s.len() as i64).unwrap_or_else(|| as_uint(data.get("nb_tracks"))),
        "duration": duration,
        "created": iso((!release.is_empty()).then(|| json!(release)).as_ref()),
        "year": as_uint(Some(&json!(year))),
        "genre": genres.first().cloned().unwrap_or_default(),
        "userRating": 0,
        "genres": genres.iter().map(|g| json!({"name": g})).collect::<Vec<_>>(),
        "musicBrainzId": "",
        "isCompilation": record_type == "compile",
        "sortName": name.to_lowercase(),
        "discTitles": [],
        "originalReleaseDate": {},
        "releaseDate": item_date(&release),
        "releaseTypes": if record_type.is_empty() { json!([]) } else { json!([record_type]) },
        "recordLabels": opt_str(data.get("label")).map(|l| json!([{"name": l}])).unwrap_or(json!([])),
        "moods": [],
        "artists": if artist_name.is_empty() { json!([]) } else { json!([{"id": artist_id, "name": artist_name}]) },
        "displayArtist": artist_name,
        "explicitStatus": if truthy(data.get("explicit_lyrics")) { "explicit" } else { "" },
        "version": "",
    });
    let map = out.as_object_mut().unwrap();
    if let Some(ts) = starred.filter(|t| *t != 0.0) {
        map.insert("starred".into(), json!(iso_ts(ts)));
    }
    if let Some(songs) = songs {
        map.insert("song".into(), Value::Array(songs));
    }
    std::mem::take(map)
}

/// A virtual artist (`ArtistID3`) from a Deezer artist object.
pub fn artist(artist_id: &str, data: &Json, albums: Option<Vec<Value>>, starred: Option<f64>) -> Json {
    let name = as_str(data.get("name"));
    let image = opt_str(data.get("picture_xl"))
        .or_else(|| opt_str(data.get("picture_big")))
        .or_else(|| opt_str(data.get("picture")))
        .unwrap_or_default();
    let mut out = json!({
        "id": artist_id,
        "name": name,
        "coverArt": artist_id,
        "albumCount": albums.as_ref().map(|a| a.len() as i64).unwrap_or_else(|| as_uint(data.get("nb_album"))),
        "artistImageUrl": image,
        "musicBrainzId": "",
        "sortName": name.to_lowercase(),
        "roles": ["artist", "albumartist"],
    });
    let map = out.as_object_mut().unwrap();
    if let Some(ts) = starred.filter(|t| *t != 0.0) {
        map.insert("starred".into(), json!(iso_ts(ts)));
    }
    if let Some(albums) = albums {
        map.insert("album".into(), Value::Array(albums));
    }
    std::mem::take(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures/navidrome_0.63.json")).unwrap()
    }

    fn keys(v: &Value) -> std::collections::BTreeSet<String> {
        v.as_object().unwrap().keys().cloned().collect()
    }

    fn types(v: &Value) -> Vec<(String, &'static str)> {
        let kind = |x: &Value| match x {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        v.as_object().unwrap().iter().map(|(k, x)| (k.clone(), kind(x))).collect()
    }

    fn meta() -> Json {
        json!({"title": "Song", "artist": "A", "artists": ["A", "B"], "album": "Alb", "album_id": 9,
               "artist_id": 3, "track": 2, "disc": 1, "year": 2020, "duration": 181.6, "isrc": "X1",
               "genre": "Pop", "explicit": true})
        .as_object()
        .cloned()
        .unwrap()
    }

    #[test]
    fn song_has_navidrome_key_set_and_types() {
        let fx = fixture();
        let real = &fx["song"];
        let ours = Value::Object(song("deezer:1", &meta(), &Json::new(), Some(&json!(1700000000.0)), Some(1700000001.0)));
        let missing: Vec<_> = keys(real).difference(&keys(&ours)).cloned().collect();
        assert!(missing.is_empty(), "missing keys {missing:?}");
        let ours_types: std::collections::HashMap<_, _> = types(&ours).into_iter().collect();
        for (k, t) in types(real) {
            if let Some(o) = ours_types.get(&k) {
                assert_eq!(*o, t, "type of {k}");
            }
        }
        assert_eq!(ours["coverArt"], "deezer-album:9");
        assert_eq!(ours["duration"], 182);
        assert_eq!(ours["explicitStatus"], "explicit");
        assert_eq!(ours["created"], "2023-11-14T22:13:20Z");
        assert_eq!(ours["artists"], json!([{"id": "deezer-artist:3", "name": "A"}, {"id": "", "name": "B"}]));
    }

    #[test]
    fn youtube_song_uses_its_own_cover() {
        let s = song("yt:dQw4w9WgXcQ", &meta(), &Json::new(), None, None);
        assert_eq!(s["coverArt"], "yt:dQw4w9WgXcQ");
        assert!(!s.contains_key("starred"));
    }

    #[test]
    fn album_and_artist_key_sets() {
        let fx = fixture();
        let data = json!({"id": 9, "title": "Alb", "artist": {"id": 3, "name": "A"}, "release_date": "2020-05-00",
                          "genres": {"data": [{"name": "Pop"}]}, "record_type": "album", "nb_tracks": 3, "label": "L"});
        let a = Value::Object(album("deezer-album:9", data.as_object().unwrap(), Some(vec![]), None));
        let missing: Vec<_> = keys(&fx["album"]).difference(&keys(&a)).cloned().collect();
        assert!(missing.is_empty(), "missing album keys {missing:?}");
        assert_eq!(a["releaseDate"], json!({"year": 2020, "month": 5}));
        assert_eq!(a["year"], 2020);
        let ar = Value::Object(artist("deezer-artist:3", json!({"name": "A", "nb_album": 4}).as_object().unwrap(), None, None));
        let missing: Vec<_> = keys(&fx["artist"]).difference(&keys(&ar)).cloned().collect();
        assert!(missing.is_empty(), "missing artist keys {missing:?}");
        assert_eq!(ar["albumCount"], 4);
    }

    #[test]
    fn iso_formats() {
        assert_eq!(iso(Some(&json!("2020-01-02"))), "2020-01-02T00:00:00Z");
        assert_eq!(iso(Some(&json!("2020-01-02T03:04:05+02:00"))), "2020-01-02T01:04:05Z");
        assert_eq!(iso(Some(&json!("2020-01-02 03:04:05"))), "2020-01-02T03:04:05Z");
        assert_eq!(iso(Some(&json!(0))), "1970-01-01T00:00:00Z");
        assert_eq!(iso(None).len(), 20);
    }
}

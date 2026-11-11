//! search2 / search3: the local library first, then the external catalog.
//!
//! Pagination contract (the first proxy broke it): clients page with
//! `songOffset` = number of songs received so far and stop at an empty page.
//! External results are added to the *first* page only; later pages continue the
//! local results, shifted by the number of external items shown on page one, and
//! end when the library is exhausted. Arpeggi used to page forever because every
//! page repeated the same external songs.
//!
//! First/later page is decided per kind: a client paging songs while it still
//! asks for `albumOffset=0` gets the shifted local songs and no repeated
//! external songs (review bug 1.3).

use super::browse::merge_with_local;
use super::{Call, Reply};
use crate::app::Services;
use crate::deezer::DeezerError;
use crate::identity::recording_key;
use crate::ids;
use crate::matching::{self, split_artists};
use crate::navidrome::NdError;
use crate::sources::YtmTrack;
use crate::subsonic::models;
use crate::subsonic::params::CREDENTIAL_KEYS;
use crate::util::{Json, as_str, as_uint, obj, opt_str};
use regex::Regex;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fmt::Display;
use std::future::Future;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

static YOUTUBE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|shorts/)|youtu\.be/|music\.youtube\.com/watch\?(?:.*&)?v=)([A-Za-z0-9_-]{11})").unwrap()
});
static PREFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)^(yt|y|youtube|sc|soundcloud)\s*:\s*(.+)$").unwrap());

pub const KINDS: [&str; 3] = ["artist", "album", "song"];
const ARTIST: usize = 0;
const ALBUM: usize = 1;
const SONG: usize = 2;

/// `(user, query, kind)` -> (when, local items on page one, external items on page one).
pub type PageMemory =
    std::collections::HashMap<(String, String, &'static str), (Instant, usize, usize)>;

/// (count, offset) per kind, in `KINDS` order.
type Counts = [(usize, usize); 3];

fn counts(call: &Call) -> Counts {
    KINDS.map(|k| {
        (
            call.params.int(&format!("{k}Count"), 20).max(0) as usize,
            call.params.int(&format!("{k}Offset"), 0).max(0) as usize,
        )
    })
}

fn first_page(c: (usize, usize)) -> bool {
    c.0 > 0 && c.1 == 0
}

fn as_list(v: Option<&Value>) -> Vec<Json> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_object().cloned()).collect(),
        Some(Value::Object(o)) => vec![o.clone()],
        _ => vec![],
    }
}

pub async fn search(s: &Arc<Services>, call: &Call) -> Reply {
    let query = call
        .params
        .get("query")
        .trim()
        .trim_matches('"')
        .trim()
        .to_string();
    if query.is_empty() || query == "*" {
        return Ok(None); // library sync: pure passthrough
    }
    let counts = counts(call);
    let key = if call.method == "search3" {
        "searchResult3"
    } else {
        "searchResult2"
    };
    let folded = query.to_lowercase();
    // Later pages of kinds that showed external items continue the local list, shifted.
    let mut overrides: Vec<(String, String)> = Vec::new();
    let mut exhausted = [false; 3];
    {
        let memory = s.search_pages.lock();
        for (i, kind) in KINDS.iter().enumerate() {
            let (count, offset) = counts[i];
            if count == 0 || offset == 0 {
                continue;
            }
            // Nothing remembered: nothing external was shown, offsets are already local.
            let Some((_, local_first, external_first)) =
                memory.get(&(call.username().to_string(), folded.clone(), *kind))
            else {
                continue;
            };
            if *local_first < count {
                exhausted[i] = true; // the first page already held every local result
            } else {
                overrides.push((
                    format!("{kind}Offset"),
                    offset.saturating_sub(*external_first).to_string(),
                ));
            }
        }
    }
    let (local, external) = if counts.iter().any(|c| first_page(*c)) {
        tokio::join!(
            local(s, call, key, &overrides),
            external(s, &query, &counts)
        )
    } else {
        (local(s, call, key, &overrides).await, External::default())
    };
    let mut local = local?;
    for (i, done) in exhausted.iter().enumerate() {
        if *done {
            local[i].clear();
        }
    }
    let result = if counts.iter().any(|c| first_page(*c)) {
        merge(s, call, &query, &counts, local, external).await?
    } else {
        KINDS
            .iter()
            .zip(local)
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| {
                (
                    k.to_string(),
                    Value::Array(v.into_iter().map(Value::Object).collect()),
                )
            })
            .collect()
    };
    Ok(Some(call.ok_with(s, key, Value::Object(result))))
}

async fn local(
    s: &Services,
    call: &Call,
    key: &str,
    overrides: &[(String, String)],
) -> Result<[Vec<Json>; 3], NdError> {
    let mut extra: Vec<(String, String)> = Vec::new();
    for (k, v) in call.params.without(&["f", "callback"]).items() {
        if CREDENTIAL_KEYS.contains(&k.as_str())
            || k == "v"
            || k == "c"
            || extra.iter().any(|(e, _)| e == k)
        {
            continue;
        }
        let value = overrides
            .iter()
            .find(|(o, _)| o == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| v.clone());
        extra.push((k.clone(), value));
    }
    for (k, v) in overrides {
        if !extra.iter().any(|(e, _)| e == k) {
            extra.push((k.clone(), v.clone()));
        }
    }
    let pairs: Vec<(&str, &str)> = extra
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    match s.navidrome.call(&call.method, &call.params, &pairs).await {
        Ok(root) => {
            let result = obj(root.get(key));
            Ok(KINDS.map(|k| as_list(result.get(k))))
        }
        Err(NdError::Upstream { code, message }) => {
            tracing::info!("local search failed: {code}: {message}");
            Ok(Default::default())
        }
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------- external
#[derive(Default)]
struct External {
    tracks: Vec<Json>,
    youtube: Vec<YtmTrack>,
    albums: Vec<Json>,
    artists: Vec<Json>,
    soundcloud: Vec<Json>,
    only_external: bool,
}

/// `fut`'s rows, or none if it failed or ran over the search budget.
async fn within<T: Default, E: Display>(
    budget: Duration,
    name: &str,
    query: &str,
    fut: impl Future<Output = Result<T, E>>,
) -> T {
    match tokio::time::timeout(budget, fut).await {
        Ok(Ok(rows)) => rows,
        Ok(Err(e)) => {
            tracing::info!("external {name} search failed for {query:?}: {e}");
            T::default()
        }
        Err(_) => {
            tracing::info!(
                "external {name} search for {query:?} exceeded the {:.1}s budget",
                budget.as_secs_f64()
            );
            T::default()
        }
    }
}

async fn external(s: &Services, query: &str, counts: &Counts) -> External {
    let settings = &s.settings;
    let budget = Duration::from_secs_f64(settings.search_budget_seconds.max(0.0));
    let (want_artists, want_albums, want_songs) = (
        first_page(counts[ARTIST]),
        first_page(counts[ALBUM]),
        first_page(counts[SONG]),
    );
    let mut out = External::default();
    if let Some(video) = YOUTUBE_URL.captures(query) {
        out.only_external = true;
        if want_songs {
            out.youtube = within(budget, "youtube", query, youtube_video(s, &video[1])).await;
        }
        return out;
    }
    if let Some(prefix) = PREFIX.captures(query) {
        out.only_external = true;
        if want_songs {
            let text = prefix[2].to_string();
            if prefix[1].to_lowercase().starts_with('y') {
                out.youtube = within(budget, "youtube", query, async {
                    Ok::<_, String>(s.finder.ytmusic(&text, None, 20).await)
                })
                .await;
            } else {
                out.soundcloud = within(budget, "soundcloud", query, async {
                    Ok::<_, String>(soundcloud(s, &text).await)
                })
                .await;
            }
        }
        return out;
    }
    let none = || async { Ok::<Vec<Json>, DeezerError>(Vec::new()) };
    let (tracks, youtube, albums, artists) = tokio::join!(
        within(budget, "tracks", query, async {
            if want_songs {
                s.deezer
                    .search_tracks(query, settings.external_songs, 0)
                    .await
            } else {
                none().await
            }
        }),
        within(budget, "youtube", query, async {
            Ok::<_, String>(if want_songs && settings.youtube_songs > 0 {
                s.finder.ytmusic(query, Some("songs"), 10).await
            } else {
                vec![]
            })
        }),
        within(budget, "albums", query, async {
            if want_albums {
                s.deezer
                    .search_albums(query, settings.external_albums)
                    .await
            } else {
                none().await
            }
        }),
        within(budget, "artists", query, async {
            if want_artists {
                s.deezer
                    .search_artists(query, settings.external_artists)
                    .await
            } else {
                none().await
            }
        }),
    );
    External {
        tracks,
        youtube,
        albums,
        artists,
        soundcloud: vec![],
        only_external: false,
    }
}

async fn youtube_video(s: &Services, video_id: &str) -> Result<Vec<YtmTrack>, String> {
    let rows = s.finder.ytmusic(video_id, None, 5).await;
    if let Some(exact) = rows.into_iter().find(|r| r.video_id == video_id) {
        return Ok(vec![exact]);
    }
    Ok(vec![YtmTrack {
        video_id: video_id.into(),
        title: video_id.into(),
        artists: vec!["YouTube".into()],
        thumbnail: Some(format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")),
        ..Default::default()
    }])
}

async fn soundcloud(s: &Services, text: &str) -> Vec<Json> {
    let mut rows = Vec::new();
    for e in s.finder.flat_search(&format!("scsearch15:{text}")).await {
        let id = as_str(e.get("id"));
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let thumbnail = e
            .get("thumbnails")
            .and_then(Value::as_array)
            .and_then(|t| t.last())
            .and_then(|t| opt_str(t.get("url")));
        let row = json!({
            "sc_id": id,
            "url": opt_str(e.get("url")).or_else(|| opt_str(e.get("webpage_url"))),
            "title": opt_str(e.get("title")).unwrap_or_else(|| id.clone()),
            "artists": [opt_str(e.get("uploader")).unwrap_or_else(|| "SoundCloud".into())],
            "duration": e.get("duration"),
            "thumbnail": thumbnail,
        });
        rows.extend(row.as_object().cloned());
    }
    rows
}

fn remember_soundcloud(s: &Services, rows: &[Json]) -> Vec<String> {
    let items: Vec<(String, Json)> = rows
        .iter()
        .map(|row| {
            let artists: Vec<String> = row.get("artists").and_then(Value::as_array).into_iter().flatten().map(|a| as_str(Some(a))).collect();
            let meta = json!({
                "source": "soundcloud", "title": row.get("title"), "artists": artists, "artist": artists.join(", "),
                "duration": row.get("duration"), "cover": row.get("thumbnail"), "url": row.get("url"),
            });
            (ids::soundcloud(as_str(row.get("sc_id"))), meta.as_object().cloned().unwrap_or_default())
        })
        .collect();
    s.store.remember_tracks(&items);
    items.into_iter().map(|(id, _)| id).collect()
}

/// Re-rank catalog hits by how much of the query they contain; drop unrelated ones.
///
/// Deezer's own ranking is popularity-driven: "юпи порезы" returned an unrelated
/// popular song first and the right track sixth. `fields` returns
/// (artist, title, album); artist+title coverage outranks coverage through the
/// album name (tribute albums), and `strict` requires full artist+title coverage.
fn relevant<T>(
    query: &str,
    rows: Vec<T>,
    fields: impl Fn(&T) -> [String; 3],
    strict: bool,
) -> Vec<T> {
    if PREFIX.is_match(query) || YOUTUBE_URL.is_match(query) {
        return rows;
    }
    let words = matching::normalize(query).split_whitespace().count();
    let floor = if words <= 2 { 1.0 } else { 0.6 };
    let mut kept: Vec<(f64, f64, usize)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let [artist, title, album] = fields(row);
        let core = matching::relevance(query, &[&artist, &title]);
        let full = core.max(matching::relevance(query, &[&artist, &title, &album]));
        if (if strict { core } else { full }) >= (if strict { 1.0 } else { floor }) {
            kept.push((full, core, index));
        }
    }
    if kept.is_empty() && !strict {
        // Transliterations and typos ("zemfira"): trust the catalog's own top hits.
        return rows.into_iter().take(5).collect();
    }
    kept.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.cmp(&b.2))
    });
    let mut slots: Vec<Option<T>> = rows.into_iter().map(Some).collect();
    kept.into_iter()
        .filter_map(|(_, _, i)| slots[i].take())
        .collect()
}

fn artist_name(row: &Json) -> String {
    as_str(obj(row.get("artist")).get("name"))
}

fn key_of(artist: &str, title: &str) -> Option<String> {
    recording_key(&split_artists(artist), title)
}

// ------------------------------------------------------------------- merge
async fn merge(
    s: &Services,
    call: &Call,
    query: &str,
    counts: &Counts,
    local: [Vec<Json>; 3],
    ext: External,
) -> Result<Json, NdError> {
    let [local_artists, local_albums, local_songs] = local;
    let only_external = ext.only_external;
    let local_songs = if only_external { vec![] } else { local_songs };
    let local_ids: HashSet<String> = local_songs.iter().map(|x| as_str(x.get("id"))).collect();
    let local_keys: HashSet<String> = local_songs
        .iter()
        .filter_map(|x| key_of(&as_str(x.get("artist")), &as_str(x.get("title"))))
        .collect();

    let tracks = relevant(
        query,
        ext.tracks,
        |r| {
            [
                artist_name(r),
                as_str(r.get("title")),
                as_str(obj(r.get("album")).get("title")),
            ]
        },
        false,
    );
    let youtube = relevant(
        query,
        ext.youtube,
        |r| {
            [
                r.artists.join(" "),
                r.title.clone(),
                r.album.clone().unwrap_or_default(),
            ]
        },
        !only_external,
    );
    let album_rows = relevant(
        query,
        ext.albums,
        |r| [artist_name(r), as_str(r.get("title")), String::new()],
        false,
    );
    let deezer_ids = s.catalog.remember_deezer(&tracks, None);
    let deezer_keys: HashSet<String> = tracks
        .iter()
        .filter_map(|r| key_of(&artist_name(r), &as_str(r.get("title"))))
        .collect();
    let youtube_rows: Vec<YtmTrack> = youtube
        .into_iter()
        .filter(|r| {
            only_external
                || recording_key(&r.artists, &r.title)
                    .is_none_or(|k| !deezer_keys.contains(&k) && !local_keys.contains(&k))
        })
        .collect();
    let mut youtube_ids = s.catalog.remember_ytmusic(&youtube_rows);
    if !only_external {
        youtube_ids.truncate(s.settings.youtube_songs);
    }
    let soundcloud_ids = remember_soundcloud(s, &ext.soundcloud);

    // Local first, then the best catalog hit, then YouTube-only songs, then the rest.
    let ordered: Vec<String> = deezer_ids
        .iter()
        .take(1)
        .chain(&youtube_ids)
        .chain(&soundcloud_ids)
        .chain(deezer_ids.iter().skip(1))
        .cloned()
        .collect();
    let presented = s.catalog.present(&ordered, &call.creds(), &local_ids).await;
    let local_shapes: Vec<(String, i64)> = local_songs
        .iter()
        .map(|x| {
            (
                matching::base_title(&as_str(x.get("title"))),
                as_uint(x.get("duration")),
            )
        })
        .collect();
    let duplicate_of_local = |x: &Json| {
        if key_of(&as_str(x.get("artist")), &as_str(x.get("title")))
            .is_some_and(|k| local_keys.contains(&k))
        {
            return true;
        }
        // Same title and length but the artist spelled differently (transliteration).
        let (title, length) = (
            matching::base_title(&as_str(x.get("title"))),
            as_uint(x.get("duration")),
        );
        local_shapes
            .iter()
            .any(|(t, d)| *t == title && length != 0 && (length - d).abs() <= 2)
    };
    let external_songs: Vec<Json> = presented
        .into_iter()
        .filter(|x| {
            let id = as_str(x.get("id"));
            !local_ids.contains(&id) && (ids::parse(&id).is_none() || !duplicate_of_local(x))
        })
        .collect();
    let local_song_count = local_songs.len();
    let songs: Vec<Json> = if counts[SONG].0 > 0 {
        local_songs.into_iter().chain(external_songs).collect()
    } else {
        vec![]
    };

    let local_album_count = local_albums.len();
    let mut album_keys: HashSet<(String, String)> = local_albums
        .iter()
        .map(|a| {
            (
                matching::normalize(&as_str(a.get("name"))),
                matching::normalize(&as_str(a.get("artist"))),
            )
        })
        .collect();
    let candidates: Vec<Json> = album_rows
        .into_iter()
        .filter(|row| {
            album_keys.insert((
                matching::normalize(&as_str(row.get("title"))),
                matching::normalize(&artist_name(row)),
            ))
        })
        .collect();
    let local_album_ids: HashSet<String> =
        local_albums.iter().map(|a| as_str(a.get("id"))).collect();
    let external_albums: Vec<Json> = merge_with_local(s, call, candidates)
        .await?
        .into_iter()
        .filter(|a| !local_album_ids.contains(&as_str(a.get("id"))))
        .collect();
    let external_album_count = external_albums.len();
    let albums: Vec<Json> = local_albums.into_iter().chain(external_albums).collect();

    let local_artist_count = local_artists.len();
    let mut names: HashSet<String> = local_artists
        .iter()
        .map(|a| matching::normalize(&as_str(a.get("name"))))
        .collect();
    let mut artists = local_artists;
    for row in &ext.artists {
        let name = matching::normalize(&as_str(row.get("name")));
        if name.is_empty() || !names.insert(name) {
            continue;
        }
        artists.push(models::artist(
            &ids::artist(as_str(row.get("id"))),
            row,
            None,
            None,
        ));
    }
    let external_artist_count = artists.len() - local_artist_count;

    let now = Instant::now();
    {
        let mut memory = s.search_pages.lock();
        let folded = query.to_lowercase();
        for (kind, local_n, extra) in [
            (
                "song",
                local_song_count,
                songs.len().saturating_sub(local_song_count),
            ),
            ("album", local_album_count, external_album_count),
            ("artist", local_artist_count, external_artist_count),
        ] {
            if extra > 0 {
                memory.insert(
                    (call.username().to_string(), folded.clone(), kind),
                    (now, local_n, extra),
                );
            }
        }
        if memory.len() > 5000 {
            memory.retain(|_, (at, _, _)| now.duration_since(*at) < Duration::from_secs(1800));
        }
    }

    let list = |v: Vec<Json>| Value::Array(v.into_iter().map(Value::Object).collect());
    let mut result = Json::new();
    if !artists.is_empty() && counts[ARTIST].0 > 0 {
        result.insert("artist".into(), list(artists));
    }
    if !albums.is_empty() && counts[ALBUM].0 > 0 {
        result.insert("album".into(), list(albums));
    }
    if !songs.is_empty() {
        result.insert("song".into(), list(songs));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_shapes() {
        assert_eq!(
            &YOUTUBE_URL
                .captures("https://youtu.be/dEvrb6N70kA?t=1")
                .unwrap()[1],
            "dEvrb6N70kA"
        );
        assert_eq!(
            &YOUTUBE_URL
                .captures("https://music.youtube.com/watch?list=x&v=dEvrb6N70kA")
                .unwrap()[1],
            "dEvrb6N70kA"
        );
        let p = PREFIX.captures("SC: some song").unwrap();
        assert_eq!((&p[1], &p[2]), ("SC", "some song"));
        assert!(PREFIX.captures("yesterday").is_none());
    }

    #[test]
    fn relevance_reranks_and_falls_back() {
        let rows = vec![
            ["Popular", "Other Song", ""],
            ["Юпи", "Порезы на руках", ""],
            ["X", "Tribute", "Юпи Порезы"],
        ];
        let f = |r: &[&str; 3]| r.map(String::from);
        let got = relevant("юпи порезы", rows.clone(), f, false);
        assert_eq!(got, [rows[1], rows[2]]);
        assert_eq!(
            relevant("zemfira", rows.clone(), f, false).len(),
            3,
            "nothing relevant: catalog order"
        );
        assert!(relevant("zemfira", rows.clone(), f, true).is_empty());
        assert_eq!(relevant("yt: anything", rows.clone(), f, true).len(), 3);
    }
}

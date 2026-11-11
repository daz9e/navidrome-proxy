//! In-process fakes of the upstream services, served by real HTTP servers on 127.0.0.1:0.

use axum::Router;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::util::Json;

pub struct Server {
    pub url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub async fn serve(router: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    Server { url, handle }
}

pub fn query_pairs(req: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(req.uri().query().unwrap_or("").as_bytes()).into_owned().collect()
}

pub fn json_response(v: Value) -> Response {
    axum::Json(v).into_response()
}

pub fn m(v: Value) -> Json {
    v.as_object().cloned().unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
pub fn dz_track(id: u64, title: &str, artist: &str, album_id: u64, album: &str, position: u64, contributors: &[&str]) -> Json {
    let mut t = json!({
        "id": id, "type": "track", "readable": true, "title": title, "title_version": "",
        "duration": 200, "rank": 1000, "explicit_lyrics": false, "md5_image": format!("md5{album_id}"),
        "artist": {"id": 1, "name": artist, "picture_xl": "https://img/1.jpg"},
        "album": {"id": album_id, "title": album, "md5_image": format!("md5{album_id}"), "release_date": "2020-05-01"},
        "track_position": position, "disk_number": 1, "isrc": format!("ISRC{id}"),
    });
    if !contributors.is_empty() {
        t["contributors"] = contributors.iter().enumerate().map(|(i, n)| json!({"id": i, "name": n, "role": "Main"})).collect();
    }
    m(t)
}

#[derive(Default)]
pub struct DeezerState {
    pub tracks: BTreeMap<u64, Json>,
    pub albums: BTreeMap<u64, Json>,
    pub artists: BTreeMap<u64, Json>,
    pub hits: Vec<String>,
    pub quota_failures: u32,
}

/// A fake Deezer API. `default()` holds the album "RAW" (100) by TVETH (1).
#[derive(Clone)]
pub struct FakeDeezer {
    pub state: Arc<Mutex<DeezerState>>,
}

impl Default for FakeDeezer {
    fn default() -> Self {
        let fake = Self::empty();
        fake.add_album(
            100,
            "RAW",
            "TVETH",
            1,
            vec![
                dz_track(4250555171, "Hella Quality", "TVETH", 100, "RAW", 1, &["TVETH", "Bato"]),
                dz_track(2, "Second", "TVETH", 100, "RAW", 2, &[]),
            ],
        );
        fake
    }
}

impl FakeDeezer {
    pub fn empty() -> Self {
        Self { state: Arc::new(Mutex::new(DeezerState::default())) }
    }

    pub fn hits(&self, path_qs: &str) -> usize {
        self.state.lock().hits.iter().filter(|h| h.as_str() == path_qs).count()
    }

    pub fn add_album(&self, id: u64, title: &str, artist: &str, artist_id: u64, tracks: Vec<Json>) {
        let mut s = self.state.lock();
        s.albums.insert(
            id,
            m(json!({
                "id": id, "title": title, "md5_image": format!("md5{id}"), "release_date": "2020-05-01",
                "nb_tracks": tracks.len(), "record_type": "album", "explicit_lyrics": false,
                "genres": {"data": [{"id": 1, "name": "Rap"}]},
                "artist": {"id": artist_id, "name": artist},
                "tracks": {"data": tracks},
            })),
        );
        for t in tracks {
            s.tracks.insert(t["id"].as_u64().unwrap(), t);
        }
        s.artists.entry(artist_id).or_insert_with(|| {
            m(json!({"id": artist_id, "name": artist, "nb_album": 1, "picture_xl": format!("https://img/{artist_id}.jpg")}))
        });
    }

    pub fn router(&self) -> Router {
        Router::new().fallback(Self::handle).with_state(self.clone())
    }

    async fn handle(State(fake): State<FakeDeezer>, req: Request) -> Response {
        let path = req.uri().path().trim_start_matches('/').to_string();
        let mut s = fake.state.lock();
        s.hits.push(req.uri().path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default());
        if s.quota_failures > 0 {
            s.quota_failures -= 1;
            return json_response(json!({"error": {"type": "Exception", "message": "Quota limit exceeded", "code": 4}}));
        }
        let q = query_pairs(&req).into_iter().find(|(k, _)| k == "q").map(|(_, v)| v.to_lowercase()).unwrap_or_default();
        let limit: usize = query_pairs(&req).into_iter().find(|(k, _)| k == "limit").and_then(|(_, v)| v.parse().ok()).unwrap_or(5);
        let words: Vec<&str> = q.split_whitespace().collect();
        let parts: Vec<&str> = path.split('/').collect();
        let id: u64 = parts.get(1).and_then(|p| p.parse().ok()).unwrap_or(0);
        let without = |v: &Json, key: &str| -> Value {
            let mut c = v.clone();
            c.remove(key);
            Value::Object(c)
        };
        let text = |v: &Value| v.as_str().unwrap_or("").to_lowercase();
        match (parts[0], parts.get(2).copied()) {
            ("search", _) => {
                let kind = parts.get(1).copied().unwrap_or("track");
                let data: Vec<Value> = match kind {
                    "track" => s
                        .tracks
                        .values()
                        .filter(|t| words.iter().all(|w| format!("{} {}", text(&t["title"]), text(&t["artist"]["name"])).contains(w)))
                        .map(|t| without(t, "contributors"))
                        .collect(),
                    "album" => s
                        .albums
                        .values()
                        .filter(|a| words.iter().all(|w| format!("{} {}", text(&a["title"]), text(&a["artist"]["name"])).contains(w)))
                        .map(|a| without(a, "tracks"))
                        .collect(),
                    _ => s.artists.values().filter(|a| !q.is_empty() && text(&a["name"]).contains(&q)).map(|a| Value::Object(a.clone())).collect(),
                };
                json_response(json!({"total": data.len(), "data": data}))
            }
            ("track", None) if s.tracks.contains_key(&id) => {
                let mut t = s.tracks[&id].clone();
                if !t.contains_key("contributors") {
                    t.insert("contributors".into(), json!([{"id": 0, "name": t["artist"]["name"], "role": "Main"}]));
                }
                json_response(Value::Object(t))
            }
            ("album", Some("tracks")) if s.albums.contains_key(&id) => {
                let data = s.albums[&id]["tracks"]["data"].clone();
                json_response(json!({"data": data}))
            }
            ("album", None) if s.albums.contains_key(&id) => json_response(Value::Object(s.albums[&id].clone())),
            ("artist", Some("albums")) if s.artists.contains_key(&id) => {
                let data: Vec<Value> =
                    s.albums.values().filter(|a| a["artist"]["id"] == json!(id)).map(|a| without(a, "tracks")).collect();
                json_response(json!({"data": data}))
            }
            ("artist", Some("top")) if s.artists.contains_key(&id) => {
                let data: Vec<Value> =
                    s.tracks.values().filter(|t| t["artist"]["id"] == json!(id)).take(limit).map(|t| Value::Object(t.clone())).collect();
                json_response(json!({"data": data}))
            }
            ("artist", Some("related")) if s.artists.contains_key(&id) => {
                let data: Vec<Value> =
                    s.artists.values().filter(|a| a["id"] != json!(id)).map(|a| Value::Object(a.clone())).collect();
                json_response(json!({"data": data}))
            }
            ("artist", None) if s.artists.contains_key(&id) => json_response(Value::Object(s.artists[&id].clone())),
            _ => json_response(json!({"error": {"type": "DataException", "message": "no data", "code": 800}})),
        }
    }
}

// ------------------------------------------------------------------ Navidrome

pub const USERS: [(&str, &str); 2] = [("alice", "alicepw"), ("bob", "bobpw")];

pub fn md5_token(password: &str, salt: &str) -> String {
    use md5::{Digest, Md5};
    format!("t={}&s={salt}", hex::encode(Md5::digest(format!("{password}{salt}"))))
}

fn nd_identity() -> Json {
    m(json!({"version": "1.16.1", "type": "navidrome", "serverVersion": "0.63.2 (fake)", "openSubsonic": true}))
}

pub fn nd_ok(payload: Value) -> Value {
    let mut root = m(json!({"status": "ok"}));
    root.extend(nd_identity());
    root.extend(m(payload));
    json!({"subsonic-response": root})
}

pub fn nd_failed(code: i64, message: &str) -> Value {
    let mut root = m(json!({"status": "failed"}));
    root.extend(nd_identity());
    root.insert("error".into(), json!({"code": code, "message": message}));
    json!({"subsonic-response": root})
}

pub fn nd_song(id: &str, title: &str, artist: &str, album: &str, album_id: &str) -> Json {
    m(json!({
        "id": id, "parent": album_id, "isDir": false, "title": title, "album": album, "artist": artist,
        "track": 1, "year": 2020, "genre": "", "coverArt": format!("mf-{id}"), "size": 1000,
        "contentType": "audio/mp4", "suffix": "m4a", "duration": 200, "bitRate": 128,
        "path": format!("{id}.m4a"), "discNumber": 1, "created": "2026-01-01T00:00:00Z",
        "albumId": album_id, "artistId": "ar1", "type": "music", "bpm": 0, "comment": "",
        "sortName": title.to_lowercase(), "mediaType": "song", "musicBrainzId": "", "isrc": [],
        "genres": [], "replayGain": {}, "channelCount": 2, "samplingRate": 44100, "bitDepth": 16,
        "moods": [], "artists": [{"id": "ar1", "name": artist}], "displayArtist": artist,
        "albumArtists": [{"id": "ar1", "name": artist}], "displayAlbumArtist": artist,
        "contributors": [], "displayComposer": "", "explicitStatus": "", "groupings": [],
        "works": [], "movements": [],
    }))
}

pub type ScanHook = Arc<dyn Fn(&mut NavidromeState) + Send + Sync>;

#[derive(Default)]
pub struct NavidromeState {
    pub songs: Vec<Json>,
    pub stars: BTreeMap<String, std::collections::BTreeSet<String>>,
    pub playlists: BTreeMap<String, Vec<String>>,
    pub scrobbles: Vec<(String, String, String)>,
    pub scans: usize,
    pub requests: Vec<(String, Vec<(String, String)>)>,
    pub audio: Vec<u8>,
    pub on_scan: Option<ScanHook>,
}

impl NavidromeState {
    pub fn song(&self, id: &str) -> Option<&Json> {
        self.songs.iter().find(|s| s["id"] == id)
    }
}

/// A tiny Navidrome: real auth, a mutable song list, stars, playlists and scans.
#[derive(Clone)]
pub struct FakeNavidrome {
    pub state: Arc<Mutex<NavidromeState>>,
}

impl Default for FakeNavidrome {
    fn default() -> Self {
        let state = NavidromeState { audio: vec![0u8; 50_000], ..Default::default() };
        Self { state: Arc::new(Mutex::new(state)) }
    }
}

fn get<'a>(q: &'a [(String, String)], key: &str) -> &'a str {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).unwrap_or("")
}

fn all<'a>(q: &'a [(String, String)], key: &str) -> Vec<&'a str> {
    q.iter().filter(|(k, _)| k == key).map(|(_, v)| v.as_str()).collect()
}

impl FakeNavidrome {
    pub fn add_song(&self, song: Json) {
        let mut s = self.state.lock();
        s.songs.retain(|x| x["id"] != song["id"]);
        s.songs.push(song);
    }

    pub fn count(&self, method: &str) -> usize {
        self.state.lock().requests.iter().filter(|(m, _)| m == method).count()
    }

    pub fn requests(&self, method: &str) -> Vec<Vec<(String, String)>> {
        self.state.lock().requests.iter().filter(|(m, _)| m == method).map(|(_, q)| q.clone()).collect()
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/rest/{method}", axum::routing::any(Self::handle))
            .fallback(|req: Request| async move {
                let headers = req.headers();
                let host = headers.get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let enc = headers.get("accept-encoding").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                Response::builder()
                    .header("X-Seen-Host", host)
                    .header("X-Seen-Encoding", enc)
                    .body(axum::body::Body::from(format!("navidrome ui {}", req.uri().path())))
                    .unwrap()
            })
            .with_state(self.clone())
    }

    fn auth(q: &[(String, String)]) -> Option<String> {
        let user = get(q, "u");
        let pw = USERS.iter().find(|(u, _)| *u == user)?.1;
        let p = get(q, "p");
        if !p.is_empty() {
            let given = match p.strip_prefix("enc:") {
                Some(h) => String::from_utf8(hex::decode(h).ok()?).ok()?,
                None => p.to_string(),
            };
            return (given == pw).then(|| user.to_string());
        }
        let (t, s) = (get(q, "t"), get(q, "s"));
        if !t.is_empty() && !s.is_empty() {
            return (md5_token(pw, s) == format!("t={t}&s={s}")).then(|| user.to_string());
        }
        None
    }

    async fn handle(
        State(fake): State<FakeNavidrome>,
        axum::extract::Path(method): axum::extract::Path<String>,
        req: Request,
    ) -> Response {
        let method = method.trim_end_matches(".view").to_string();
        let mut q = query_pairs(&req);
        let range = req.headers().get("range").and_then(|v| v.to_str().ok()).map(String::from);
        if req.method() == axum::http::Method::POST {
            let body = axum::body::to_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
            q.extend(url::form_urlencoded::parse(&body).into_owned());
        }
        let mut s = fake.state.lock();
        s.requests.push((method.clone(), q.clone()));
        let user = Self::auth(&q);
        if method == "ping" && get(&q, "u").is_empty() {
            return json_response(nd_ok(json!({})));
        }
        let Some(user) = user else { return json_response(nd_failed(40, "Wrong username or password")) };
        let id = get(&q, "id").to_string();
        match method.as_str() {
            "ping" => json_response(nd_ok(json!({}))),
            "search2" | "search3" => {
                let query = get(&q, "query").trim_matches('"').to_lowercase();
                let count: usize = get(&q, "songCount").parse().unwrap_or(20);
                let offset: usize = get(&q, "songOffset").parse().unwrap_or(0);
                let found: Vec<Value> = s
                    .songs
                    .iter()
                    .filter(|x| {
                        let hay = format!("{} {} {}", x["title"].as_str().unwrap_or(""), x["artist"].as_str().unwrap_or(""), x["album"].as_str().unwrap_or("")).to_lowercase();
                        query.split_whitespace().all(|w| hay.contains(w))
                    })
                    .skip(offset)
                    .take(count)
                    .map(|x| Value::Object(x.clone()))
                    .collect();
                let key = if method == "search3" { "searchResult3" } else { "searchResult2" };
                let inner = if count > 0 { json!({"song": found}) } else { json!({}) };
                json_response(nd_ok(json!({ key: inner })))
            }
            "getSong" => match s.song(&id) {
                Some(song) => json_response(nd_ok(json!({"song": song}))),
                None => json_response(nd_failed(70, "Song not found")),
            },
            "getStarred" | "getStarred2" => {
                let key = if method == "getStarred2" { "starred2" } else { "starred" };
                let songs: Vec<Value> = s
                    .stars
                    .get(&user)
                    .into_iter()
                    .flatten()
                    .filter_map(|i| s.song(i))
                    .map(|x| {
                        let mut x = x.clone();
                        x.insert("starred".into(), json!("2026-01-01T00:00:00Z"));
                        Value::Object(x)
                    })
                    .collect();
                let inner = if songs.is_empty() { json!({}) } else { json!({"song": songs}) };
                json_response(nd_ok(json!({ key: inner })))
            }
            "star" | "unstar" => {
                let ids = all(&q, "id");
                if ids.iter().any(|i| s.song(i).is_none()) {
                    return json_response(nd_failed(70, "Song not found"));
                }
                let target = s.stars.entry(user).or_default();
                for i in ids {
                    if method == "star" {
                        target.insert(i.to_string());
                    } else {
                        target.remove(i);
                    }
                }
                json_response(nd_ok(json!({})))
            }
            "scrobble" => {
                let ids: Vec<String> = all(&q, "id").into_iter().map(String::from).collect();
                if ids.iter().any(|i| s.song(i).is_none()) {
                    return json_response(nd_failed(70, "not found"));
                }
                let submission = match get(&q, "submission") {
                    "" => "true".to_string(),
                    v => v.to_string(),
                };
                for i in ids {
                    s.scrobbles.push((user.clone(), i, submission.clone()));
                }
                json_response(nd_ok(json!({})))
            }
            "startScan" => {
                s.scans += 1;
                if let Some(hook) = s.on_scan.clone() {
                    hook(&mut s);
                }
                let n = s.songs.len();
                json_response(nd_ok(json!({"scanStatus": {"scanning": true, "count": n}})))
            }
            "updatePlaylist" => {
                let ids: Vec<String> = all(&q, "songIdToAdd").into_iter().map(String::from).collect();
                if ids.iter().any(|i| s.song(i).is_none()) {
                    return json_response(nd_failed(70, "not found"));
                }
                s.playlists.entry(get(&q, "playlistId").to_string()).or_default().extend(ids);
                json_response(nd_ok(json!({})))
            }
            "getAlbum" => {
                let songs: Vec<Value> = s.songs.iter().filter(|x| x["albumId"] == id).map(|x| Value::Object(x.clone())).collect();
                if songs.is_empty() {
                    return json_response(nd_failed(70, "Album not found"));
                }
                let album = json!({
                    "id": id, "name": songs[0]["album"], "artist": songs[0]["artist"], "artistId": "ar1",
                    "coverArt": format!("al-{id}"), "songCount": songs.len(), "duration": 0, "song": songs,
                });
                json_response(nd_ok(json!({"album": album})))
            }
            "getTopSongs" => json_response(nd_ok(json!({"topSongs": {}}))),
            "createPlaylist" => {
                let pid = match get(&q, "playlistId") {
                    "" => format!("pl{}", s.playlists.len() + 1),
                    p => p.to_string(),
                };
                let songs: Vec<String> = all(&q, "songId").into_iter().map(String::from).collect();
                s.playlists.insert(pid.clone(), songs);
                json_response(nd_ok(json!({"playlist": {"id": pid, "name": get(&q, "name"), "entry": []}})))
            }
            "getPlaylist" => match s.playlists.get(&id) {
                None => json_response(nd_failed(70, "not found")),
                Some(pl) => {
                    let entries: Vec<Value> = pl.iter().filter_map(|i| s.song(i)).map(|x| Value::Object(x.clone())).collect();
                    json_response(nd_ok(json!({"playlist": {"id": id, "name": "p", "songCount": entries.len(), "duration": 0, "entry": entries}})))
                }
            },
            "stream" | "download" => {
                if s.song(&id).is_none() {
                    return json_response(nd_failed(70, "not found"));
                }
                let data = s.audio.clone();
                if let Some(rng) = range {
                    let spec = rng.trim_start_matches("bytes=");
                    let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                    let start: usize = a.parse().unwrap_or(0);
                    let end: usize = b.parse().unwrap_or(data.len() - 1);
                    return Response::builder()
                        .status(206)
                        .header("Content-Type", "audio/mp4")
                        .header("Content-Range", format!("bytes {start}-{end}/{}", data.len()))
                        .header("Accept-Ranges", "bytes")
                        .body(axum::body::Body::from(data[start..=end].to_vec()))
                        .unwrap();
                }
                Response::builder()
                    .header("Content-Type", "audio/mp4")
                    .header("Accept-Ranges", "bytes")
                    .body(axum::body::Body::from(data))
                    .unwrap()
            }
            _ => {
                let params: Json = q.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
                json_response(nd_ok(json!({"echo": {"method": method, "params": Value::Object(params).to_string()}})))
            }
        }
    }
}

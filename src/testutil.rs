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

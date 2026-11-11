//! End-to-end behaviour of the proxy against fake upstreams (real ffmpeg, tags and files).

use super::*;
use crate::audio::tests::{have, need};
use crate::pipeline::tests::{FakeFetcher, cand};
use crate::sources::YtmTrack;
use crate::sources::tests::FakeBackend;
use crate::testutil::{
    FakeDeezer, FakeNavidrome, NavidromeState, Server, dz_track, md5_token, nd_song, serve,
};
use crate::util::Json;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

const ALICE: &str = "u=alice&p=alicepw&v=1.16.1&c=Arpeggi&f=json";
const BOB: &str = "u=bob&p=bobpw&v=1.16.1&c=Arpeggi&f=json";

/// A Navidrome scan: every audio file becomes a song (id from path, real size and length).
fn index_music(music: PathBuf) -> crate::testutil::ScanHook {
    Arc::new(move |state: &mut NavidromeState| {
        for path in crate::library::files_under(&music) {
            let suffix = crate::audio::suffix_of(&path);
            if !(suffix == "m4a" || suffix == "mp3")
                || path
                    .components()
                    .any(|c| c.as_os_str() == ".navidrome-proxy-staging")
            {
                continue;
            }
            let tags = crate::tagging::read_tags(&path);
            let first = |k: &str, d: &str| {
                tags.get(k)
                    .and_then(|v| v.first())
                    .cloned()
                    .unwrap_or_else(|| d.to_string())
            };
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let (title, artist, album) = (
                first("title", stem),
                first("artist", "?"),
                first("album", "[Unknown Album]"),
            );
            let rel = path.strip_prefix(&music).unwrap().display().to_string();
            use md5::{Digest, Md5};
            let id = format!("L{}", &hex::encode(Md5::digest(rel.as_bytes()))[..10]);
            let duration = lofty::read_from_path(&path)
                .map(|f| lofty::file::AudioFile::properties(&f).duration().as_secs())
                .unwrap_or(0);
            let mut song = nd_song(&id, &title, &artist, &album, &format!("AL-{album}"));
            song.insert(
                "size".into(),
                json!(std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)),
            );
            song.insert(
                "path".into(),
                json!(format!("{artist}/{album}/{title}.{suffix}")),
            );
            song.insert("duration".into(), json!(duration));
            state.songs.retain(|s| s["id"] != json!(id));
            state.songs.push(song);
        }
    })
}

fn local_song(id: &str, title: &str, artist: &str, duration: u64) -> Json {
    let mut s = nd_song(id, title, artist, "Local Album", "AL-local");
    s.insert("duration".into(), json!(duration));
    s
}

struct World {
    url: String,
    http: reqwest::Client,
    s: Arc<Services>,
    navidrome: FakeNavidrome,
    deezer: FakeDeezer,
    fetcher: Arc<FakeFetcher>,
    _servers: Vec<Server>,
    _dir: tempfile::TempDir,
}

impl Drop for World {
    fn drop(&mut self) {
        self.s.background.cancel();
        self.s.pipeline.background.cancel();
        self.s.intents.background.cancel();
    }
}

async fn world() -> World {
    world_with(FakeBackend::default()).await
}

async fn world_with(backend: FakeBackend) -> World {
    let dir = tempfile::tempdir().unwrap();
    let music = dir.path().join("music");
    std::fs::create_dir_all(&music).unwrap();
    let navidrome = FakeNavidrome::default();
    navidrome.state.lock().on_scan = Some(index_music(music.clone()));
    let deezer = FakeDeezer::empty();
    let track = |id, title, position, contributors: &[&str]| {
        let mut t = dz_track(id, title, "TVETH", 100, "RAW", position, contributors);
        t.insert("duration".into(), json!(30));
        t
    };
    deezer.add_album(
        100,
        "RAW",
        "TVETH",
        1,
        vec![
            track(1001, "Hella Quality", 1, &["TVETH", "Bato"]),
            track(1002, "Second Song", 2, &[]),
            track(1003, "Third Song", 3, &[]),
        ],
    );
    let nav_server = serve(navidrome.router()).await;
    let dz_server = serve(deezer.router()).await;
    let env: HashMap<String, String> = [
        ("NAVIDROME_URL", nav_server.url.clone()),
        ("MUSIC_DIR", music.display().to_string()),
        ("DATA_DIR", dir.path().join("data").display().to_string()),
        ("LYRICS", "false".into()),
        ("SCAN_DEBOUNCE_SECONDS", "0.05".into()),
        ("STREAM_WAIT_SECONDS", "90".into()),
        ("SEARCH_BUDGET_SECONDS", "3".into()),
        ("DEEZER_REQUESTS_PER_SECOND", "200".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let fetcher = Arc::new(FakeFetcher::default());
    let overrides = Overrides {
        deezer_url: dz_server.url.clone(),
        backend: Some(Arc::new(backend)),
        fetcher: Some(fetcher.clone()),
        ..Default::default()
    };
    let s = build_services(Settings::from_env(&env), overrides).unwrap();
    startup(&s).await.unwrap();
    let server = serve(router(s.clone())).await;
    World {
        url: server.url.clone(),
        http: reqwest::Client::new(),
        s,
        navidrome,
        deezer,
        fetcher,
        _servers: vec![nav_server, dz_server, server],
        _dir: dir,
    }
}

impl World {
    async fn get(
        &self,
        method: &str,
        query: &str,
        auth: &str,
        headers: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut req = self
            .http
            .get(format!("{}/rest/{method}.view?{auth}&{query}", self.url));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.send().await.unwrap()
    }

    async fn json_as(&self, method: &str, query: &str, auth: &str) -> Value {
        let resp = self.get(method, query, auth, &[]).await;
        assert_eq!(resp.status(), 200);
        resp.json::<Value>().await.unwrap()["subsonic-response"].clone()
    }

    async fn json(&self, method: &str, query: &str) -> Value {
        self.json_as(method, query, ALICE).await
    }

    fn found(&self, title: &str, reference: &str) {
        self.fetcher
            .results
            .lock()
            .insert(title.into(), vec![cand(reference, "youtube")]);
    }

    fn local_id(&self, track_id: &str) -> Option<String> {
        self.s.store.get_track(track_id).and_then(|t| t.local_id)
    }

    /// Play `track_id` once (downloads it) and wait until Navidrome indexed it.
    async fn download(&self, track_id: &str, title: &str, reference: &str) -> String {
        self.found(title, reference);
        self.get("stream", &format!("id={track_id}"), ALICE, &[])
            .await
            .bytes()
            .await
            .unwrap();
        until(|| self.local_id(track_id)).await
    }

    fn requests(&self, method: &str) -> Vec<Vec<(String, String)>> {
        self.navidrome.requests(method)
    }
}

fn all(q: &[(String, String)], key: &str) -> Vec<String> {
    q.iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .collect()
}

fn ids(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .map(|x| crate::util::as_str(x.get("id")))
        .collect()
}

async fn until<T>(mut probe: impl FnMut() -> Option<T>) -> T {
    for _ in 0..1000 {
        if let Some(v) = probe() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not reached");
}

fn ytm(id: &str, title: &str, duration: f64) -> YtmTrack {
    YtmTrack {
        video_id: id.into(),
        title: title.into(),
        artists: vec!["TVETH".into()],
        duration: Some(duration),
        kind: "song".into(),
        ..Default::default()
    }
}

// ------------------------------------------------------------------ plumbing
#[tokio::test]
async fn unknown_endpoints_and_non_api_paths_pass_through() {
    let w = world().await;
    assert_eq!(
        w.json("getMusicFolders", "").await["echo"]["method"],
        "getMusicFolders"
    );
    let ui = w.http.get(format!("{}/app/", w.url)).send().await.unwrap();
    assert_eq!(ui.text().await.unwrap(), "navidrome ui /app/");
}

#[tokio::test]
async fn wrong_credentials_on_proxy_endpoint_is_a_subsonic_error() {
    let w = world().await;
    let root = w
        .json_as("search3", "query=hella", "u=alice&p=wrong&f=json")
        .await;
    assert_eq!(
        (root["status"].as_str(), root["error"]["code"].as_i64()),
        (Some("failed"), Some(40))
    );
}

#[tokio::test]
async fn form_post_parameters_are_supported() {
    let w = world().await;
    let resp = w
        .http
        .post(format!("{}/rest/getSong.view", w.url))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("u=alice&p=alicepw&f=json&id=deezer:1001")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.json::<Value>().await.unwrap()["subsonic-response"]["song"]["title"],
        "Hella Quality"
    );
}

#[tokio::test]
async fn xml_responses_and_redacted_access_log_shape() {
    let w = world().await;
    let body = w
        .get(
            "getSong",
            "id=deezer:1001",
            "u=alice&p=alicepw&v=1.16.1&c=t",
            &[],
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("<song ") && body.contains("xmlns=\"http://subsonic.org/restapi\""),
        "{body}"
    );
}

#[tokio::test]
async fn health_and_status() {
    let w = world().await;
    let health: Value = w
        .http
        .get(format!("{}/proxy/v1/health", w.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health, json!({"ok": true}));
    let denied = w
        .http
        .get(format!("{}/proxy/v1/status?u=alice&p=nope", w.url))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 401);
    let status: Value = w
        .http
        .get(format!("{}/proxy/v1/status?u=alice&p=alicepw", w.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status["pending"],
        json!({"stars": 0, "scrobbles": 0, "playlist_adds": 0})
    );
}

// -------------------------------------------------------------------- search
#[tokio::test]
async fn search_merges_local_catalog_and_youtube() {
    let backend = FakeBackend {
        ytm: vec![
            ytm("AAAAAAAAAAA", "Hella Quality", 30.0),
            ytm("BBBBBBBBBBB", "Hella Quality Pt. 2", 40.0),
        ],
        ..Default::default()
    };
    let w = world_with(backend).await;
    w.navidrome
        .add_song(local_song("L1", "Hella Quality Live Bootleg", "TVETH", 200));
    let root = w
        .json(
            "search3",
            "query=hella%20quality&songCount=50&albumCount=5&artistCount=5",
        )
        .await;
    let songs = &root["searchResult3"]["song"];
    assert_eq!(ids(songs), ["L1", "deezer:1001", "yt:BBBBBBBBBBB"]);
    assert_eq!(
        songs[1]["artists"],
        json!([{"id": "deezer-artist:1", "name": "TVETH"}])
    );
}

#[tokio::test]
async fn search_returns_catalog_albums_and_artists() {
    let w = world().await;
    let root = w
        .json(
            "search3",
            "query=tveth&songCount=0&albumCount=5&artistCount=5",
        )
        .await;
    assert_eq!(ids(&root["searchResult3"]["album"]), ["deezer-album:100"]);
    assert_eq!(ids(&root["searchResult3"]["artist"]), ["deezer-artist:1"]);
}

#[tokio::test]
async fn search_drops_external_twins_of_local_songs_with_other_spelling() {
    let w = world_with(FakeBackend {
        ytm: vec![ytm("AAAAAAAAAAA", "Hella Quality", 31.0)],
        ..Default::default()
    })
    .await;
    w.navidrome
        .add_song(local_song("L1", "Hella Quality", "ТВЕТ", 30));
    let root = w
        .json(
            "search3",
            "query=hella%20quality&songCount=50&albumCount=0&artistCount=0",
        )
        .await;
    assert_eq!(ids(&root["searchResult3"]["song"]), ["L1"]);
}

#[tokio::test]
async fn search_pagination_ends_instead_of_repeating_external_results() {
    let w = world().await;
    for i in 0..3 {
        w.navidrome.add_song(local_song(
            &format!("L{i}"),
            &format!("Hella Quality {i}"),
            "Local",
            200,
        ));
    }
    let first = w
        .json(
            "search3",
            "query=hella&songCount=2000&songOffset=0&albumCount=0&artistCount=0",
        )
        .await;
    let got = ids(&first["searchResult3"]["song"]);
    assert_eq!(got.len(), 4, "3 local + 1 external");
    let second = w
        .json(
            "search3",
            &format!(
                "query=hella&songCount=2000&songOffset={}&albumCount=0&artistCount=0",
                got.len()
            ),
        )
        .await;
    assert!(ids(&second["searchResult3"]["song"]).is_empty());
}

#[tokio::test]
async fn search_pagination_continues_local_results() {
    let w = world().await;
    for i in 0..5 {
        w.navidrome.add_song(local_song(
            &format!("L{i}"),
            &format!("Hella Quality {i}"),
            "Local",
            200,
        ));
    }
    let first = w
        .json(
            "search3",
            "query=hella&songCount=2&albumCount=0&artistCount=0",
        )
        .await;
    let got = ids(&first["searchResult3"]["song"]);
    assert_eq!(got, ["L0", "L1", "deezer:1001"]);
    let second = w
        .json(
            "search3",
            &format!(
                "query=hella&songCount=2&songOffset={}&albumCount=0&artistCount=0",
                got.len()
            ),
        )
        .await;
    assert_eq!(ids(&second["searchResult3"]["song"]), ["L2", "L3"]);
    // Bug 1.3: a client that keeps asking for albums at offset 0 still pages songs correctly.
    let mixed = w
        .json(
            "search3",
            &format!(
                "query=hella&songCount=2&songOffset={}&albumCount=5&albumOffset=0&artistCount=0",
                got.len()
            ),
        )
        .await;
    assert_eq!(ids(&mixed["searchResult3"]["song"]), ["L2", "L3"]);
}

#[tokio::test]
async fn empty_query_sync_is_passthrough() {
    let w = world().await;
    w.navidrome.add_song(local_song("L1", "A", "B", 200));
    let root = w.json("search3", "query=&songCount=10").await;
    assert_eq!(ids(&root["searchResult3"]["song"]), ["L1"]);
    assert!(
        !w.deezer
            .state
            .lock()
            .hits
            .iter()
            .any(|h| h.starts_with("/search"))
    );
}

#[tokio::test]
async fn prefixed_queries_search_only_youtube_or_soundcloud() {
    let backend = FakeBackend {
        ytm: vec![ytm("CCCCCCCCCCC", "Anything", 100.0)],
        flat: vec![
            (
                "scsearch15:".into(),
                vec![crate::testutil::m(
                    json!({"id": 77, "title": "Mix", "uploader": "DJ", "duration": 300, "url": "https://soundcloud.com/dj/mix"}),
                )],
            ),
            (
                "https://www.youtube.com/watch?v=EEEEEEEEEEE".into(),
                vec![crate::testutil::m(
                    json!({"id": "EEEEEEEEEEE", "title": "FORTUNA 812 - Black Water (slowed)", "channel": "slowed vibes", "duration": 190}),
                )],
            ),
        ],
        ..Default::default()
    };
    let w = world_with(backend).await;
    let yt = w
        .json(
            "search3",
            "query=yt:%20anything&songCount=10&albumCount=0&artistCount=0",
        )
        .await;
    assert_eq!(ids(&yt["searchResult3"]["song"]), ["yt:CCCCCCCCCCC"]);
    let sc = w
        .json(
            "search3",
            "query=sc:%20mix&songCount=10&albumCount=0&artistCount=0",
        )
        .await;
    assert_eq!(ids(&sc["searchResult3"]["song"]), ["sc:77"]);
    assert_eq!(w.s.store.get_track("sc:77").unwrap().artist(), "DJ");
    let video = w
        .json(
            "search3",
            "query=https://youtu.be/DDDDDDDDDDD&songCount=10&albumCount=0&artistCount=0",
        )
        .await;
    assert_eq!(ids(&video["searchResult3"]["song"]), ["yt:DDDDDDDDDDD"]);
    // Unknown to YouTube Music: the title and channel come from yt-dlp.
    let meta = w.s.store.get_track("yt:EEEEEEEEEEE");
    assert!(meta.is_none());
    w.json(
        "search3",
        "query=https://youtu.be/EEEEEEEEEEE?list=RDx&songCount=10&albumCount=0&artistCount=0",
    )
    .await;
    let track = w.s.store.get_track("yt:EEEEEEEEEEE").unwrap();
    assert_eq!(
        (track.title(), track.artist(), track.duration()),
        (
            "FORTUNA 812 - Black Water (slowed)".to_string(),
            "slowed vibes".to_string(),
            190.0
        )
    );
}

// ----------------------------------------------------------------- browsing
#[tokio::test]
async fn virtual_album_and_artist() {
    let w = world().await;
    let album = &w.json("getAlbum", "id=deezer-album:100").await["album"];
    assert_eq!(album["songCount"], 3);
    let tracks: Vec<i64> = album["song"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["track"].as_i64().unwrap())
        .collect();
    assert_eq!(tracks, [1, 2, 3]);
    assert_eq!(album["song"][0]["artist"], "TVETH, Bato");
    let artist = &w.json("getArtist", "id=deezer-artist:1").await["artist"];
    assert_eq!(ids(&artist["album"]), ["deezer-album:100"]);
    let info = &w.json("getArtistInfo2", "id=deezer-artist:1").await["artistInfo2"];
    assert!(!info["largeImageUrl"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_catalog_ids_are_not_found() {
    let w = world().await;
    let root = w.json("getArtist", "id=deezer-album:100").await;
    assert_eq!(
        (root["status"].as_str(), root["error"]["code"].as_i64()),
        (Some("failed"), Some(70))
    );
    assert_eq!(
        w.json("getArtist", "id=deezer-artist:999").await["status"],
        "failed"
    );
    assert_eq!(w.json("getSong", "id=deezer:999").await["status"], "failed");
}

#[tokio::test]
async fn album_info_and_wrong_kinds_are_empty_not_errors() {
    let w = world().await;
    let info = &w.json("getAlbumInfo2", "id=deezer-album:100").await["albumInfo"];
    assert!(
        info["largeImageUrl"]
            .as_str()
            .unwrap()
            .starts_with("https://")
    );
    assert_eq!(
        w.json("getAlbumInfo2", "id=deezer-artist:1").await["albumInfo"],
        json!({})
    );
    assert_eq!(
        w.json("getArtistInfo", "id=deezer-album:100").await["artistInfo"],
        json!({})
    );
}

#[tokio::test]
async fn top_and_similar_songs_come_from_the_catalog() {
    let w = world().await;
    let root = w.json("getTopSongs", "artist=TVETH&count=2").await;
    assert_eq!(
        ids(&root["topSongs"]["song"]),
        ["deezer:1001", "deezer:1002"]
    );
    let similar = w.json("getSimilarSongs2", "id=deezer-artist:1").await;
    assert!(ids(&similar["similarSongs2"]["song"]).contains(&"deezer:1001".to_string()));
    assert_eq!(
        w.json("getSimilarSongs2", "id=deezer:1001").await["similarSongs2"],
        json!({})
    );
}

#[tokio::test]
async fn lyrics_of_an_undownloaded_track_are_empty() {
    let w = world().await;
    assert_eq!(
        w.json("getLyricsBySongId", "id=deezer:1003").await["lyricsList"],
        json!({})
    );
}

// ----------------------------------------------------------------- playback
#[tokio::test]
async fn first_play_downloads_then_streams_with_ranges() {
    need!("ffmpeg");
    let w = world().await;
    w.found("Hella Quality", "dEvrb6N70kA");
    let resp = w
        .get("stream", "id=deezer:1001", ALICE, &[("Range", "bytes=0-1")])
        .await;
    assert_eq!(resp.status(), 206);
    let range = resp.headers()["content-range"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(range.starts_with("bytes 0-1/"));
    assert_eq!(resp.headers()["content-type"], "audio/mp4");
    let total: usize = range.rsplit('/').next().unwrap().parse().unwrap();
    let body = w
        .get("stream", "id=deezer:1001", ALICE, &[])
        .await
        .bytes()
        .await
        .unwrap();
    let find = |needle: &[u8]| body.windows(4).position(|x| x == needle).unwrap();
    assert_eq!(body.len(), total);
    assert!(find(b"moov") < find(b"mdat"), "faststart, no DASH");
    assert_eq!(*w.fetcher.fetched.lock(), ["dEvrb6N70kA"]);
    let song = &w.json("getSong", "id=deezer:1001").await["song"];
    assert_eq!(
        (song["size"].as_u64(), song["duration"].as_i64()),
        (Some(total as u64), Some(30))
    );
    // Other tracks of the album are prefetched in the priority lane.
    until(|| w.s.store.get_job("deezer:1002", "download")).await;
}

#[tokio::test]
async fn unavailable_track_returns_error_quickly() {
    let w = world().await;
    let resp = w.json("stream", "id=deezer:1002").await;
    assert_eq!(resp["status"], "failed");
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no matching audio")
    );
}

#[tokio::test]
async fn downloaded_track_becomes_local_everywhere() {
    need!("ffmpeg");
    let w = world().await;
    let local_id = w
        .download("deezer:1001", "Hella Quality", "dEvrb6N70kA")
        .await;
    let root = w
        .json(
            "search3",
            "query=hella&songCount=20&albumCount=0&artistCount=0",
        )
        .await;
    assert_eq!(
        ids(&root["searchResult3"]["song"]),
        std::slice::from_ref(&local_id),
        "no virtual twin"
    );
    let album = &w.json("getAlbum", "id=deezer-album:100").await["album"];
    assert_eq!(
        ids(&album["song"]),
        [local_id, "deezer:1002".into(), "deezer:1003".into()]
    );
}

#[tokio::test]
async fn local_album_is_completed_with_missing_catalog_tracks() {
    need!("ffmpeg");
    let w = world().await;
    let local_id = w
        .download("deezer:1001", "Hella Quality", "dEvrb6N70kA")
        .await;
    let album_id = w.navidrome.state.lock().song(&local_id).unwrap()["albumId"]
        .as_str()
        .unwrap()
        .to_string();
    until(|| w.s.deezer.cached_album_tracks("100")).await;
    let album = &w.json("getAlbum", &format!("id={album_id}")).await["album"];
    assert_eq!(
        ids(&album["song"]),
        [local_id, "deezer:1002".into(), "deezer:1003".into()]
    );
    assert_eq!(album["songCount"], 3);
    let before = w.navidrome.count("getAlbum");
    w.json("getAlbum", &format!("id={album_id}")).await;
    assert_eq!(
        w.navidrome.count("getAlbum"),
        before + 1,
        "bug 1.5: one upstream call per open"
    );
}

#[tokio::test]
async fn transcoding_goes_to_navidrome_raw_is_served_by_the_proxy() {
    need!("ffmpeg");
    let w = world().await;
    let local_id = w
        .download("deezer:1001", "Hella Quality", "dEvrb6N70kA")
        .await;
    let resp = w
        .get("stream", "id=deezer:1001&format=mp3", ALICE, &[])
        .await;
    assert_eq!(resp.status(), 200);
    let streamed = w.requests("stream");
    let last = streamed.last().unwrap();
    assert_eq!(
        (all(last, "id"), all(last, "format")),
        (vec![local_id], vec!["mp3".to_string()])
    );
    let before = w.navidrome.count("stream");
    let raw = w
        .get("stream", "id=deezer:1001&format=raw", ALICE, &[])
        .await;
    assert_eq!(
        (
            raw.status().as_u16(),
            raw.headers()["content-type"].to_str().unwrap()
        ),
        (200, "audio/mp4")
    );
    assert_eq!(w.navidrome.count("stream"), before);
    let download = w.get("download", "id=deezer:1001", ALICE, &[]).await;
    assert!(
        download.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment; filename*=UTF-8''")
    );
}

#[tokio::test]
async fn stream_of_an_album_id_is_passed_through() {
    let w = world().await;
    let resp = w.get("stream", "id=deezer-album:100", ALICE, &[]).await;
    assert_eq!(
        resp.status(),
        200,
        "not a track: Navidrome answers (here: song not found)"
    );
    assert_eq!(w.navidrome.count("stream"), 1);
}

// -------------------------------------------------------------------- likes
#[tokio::test]
async fn like_shows_immediately_then_lands_in_navidrome() {
    need!("ffmpeg");
    let w = world().await;
    w.found("Second Song", "SSSSSSSSSSS");
    assert_eq!(w.json("star", "id=deezer:1002").await["status"], "ok");
    let starred = &w.json("getStarred2", "").await["starred2"]["song"];
    assert_eq!(ids(starred), ["deezer:1002"]);
    assert!(starred[0]["starred"].as_str().unwrap().ends_with('Z'));
    assert!(
        w.json_as("getStarred2", "", BOB).await["starred2"]
            .get("song")
            .is_none(),
        "other users never see it"
    );
    let local_id = until(|| w.local_id("deezer:1002")).await;
    until(|| {
        w.s.store
            .pending_stars(Some("alice"))
            .is_empty()
            .then_some(())
    })
    .await;
    assert_eq!(
        w.navidrome.state.lock().stars["alice"],
        BTreeSet::from([local_id.clone()])
    );
    assert_eq!(
        ids(&w.json("getStarred2", "").await["starred2"]["song"]),
        [local_id]
    );
}

#[tokio::test]
async fn unstar_before_download_forgets_intent() {
    let w = world().await;
    w.json("star", "id=deezer:1003").await;
    w.json("unstar", "id=deezer:1003").await;
    assert!(w.s.store.pending_stars(Some("alice")).is_empty());
}

#[tokio::test]
async fn mixed_star_forwards_local_ids() {
    let w = world().await;
    w.navidrome.add_song(local_song("L1", "Local", "X", 100));
    w.json("star", "id=L1&id=deezer:1003").await;
    assert_eq!(
        w.navidrome.state.lock().stars["alice"],
        BTreeSet::from(["L1".to_string()])
    );
    let pending: Vec<String> =
        w.s.store
            .pending_stars(Some("alice"))
            .into_iter()
            .map(|(_, t, _)| t)
            .collect();
    assert_eq!(pending, ["deezer:1003"]);
}

#[tokio::test]
async fn album_like_queues_every_track_and_shows_as_starred() {
    let w = world().await;
    assert_eq!(
        w.json("star", "albumId=deezer-album:100").await["status"],
        "ok"
    );
    for n in [1001, 1002, 1003] {
        let job =
            w.s.store
                .get_job(&format!("deezer:{n}"), "download")
                .unwrap();
        assert_eq!(
            (job.priority, job.reason.as_deref()),
            (2, Some("like-album:alice"))
        );
    }
    assert_eq!(
        ids(&w.json("getStarred2", "").await["starred2"]["album"]),
        ["deezer-album:100"]
    );
    w.json("unstar", "albumId=deezer-album:100").await;
    assert!(w.s.store.pending_stars(Some("alice")).is_empty());
}

#[tokio::test]
async fn artist_like_is_kept_by_the_proxy_and_xml_is_passed_through() {
    let w = world().await;
    w.json("star", "artistId=deezer-artist:1").await;
    let starred = &w.json("getStarred2", "").await["starred2"];
    assert_eq!(ids(&starred["artist"]), ["deezer-artist:1"]);
    assert!(starred["artist"][0]["starred"].is_string());
    let xml = w
        .get(
            "getStarred2",
            "f=xml",
            "u=alice&p=alicepw&v=1.16.1&c=t",
            &[],
        )
        .await;
    assert_eq!(xml.status(), 200);
    assert!(!xml.text().await.unwrap().contains("deezer-artist"));
    w.json("unstar", "artistId=deezer-artist:1").await;
    assert!(w.s.store.pending_stars(Some("alice")).is_empty());
}

#[tokio::test]
async fn ratings_reach_navidrome_only_for_downloaded_tracks() {
    need!("ffmpeg");
    let w = world().await;
    assert_eq!(
        w.json("setRating", "id=deezer:1003&rating=5").await["status"],
        "ok"
    );
    assert_eq!(
        w.navidrome.count("setRating"),
        0,
        "not downloaded: accepted and dropped"
    );
    let local_id = w
        .download("deezer:1001", "Hella Quality", "dEvrb6N70kA")
        .await;
    w.json("setRating", "id=deezer:1001&rating=4").await;
    let sent = w.requests("setRating");
    assert_eq!(
        (all(&sent[0], "id"), all(&sent[0], "rating")),
        (vec![local_id], vec!["4".to_string()])
    );
}

// ------------------------------------------------------ scrobbles & queues
#[tokio::test]
async fn scrobble_of_external_track_is_delivered_after_download() {
    need!("ffmpeg");
    let w = world().await;
    assert_eq!(
        w.json(
            "scrobble",
            "id=deezer:1001&time=1700000000000&submission=true"
        )
        .await["status"],
        "ok"
    );
    let local_id = w
        .download("deezer:1001", "Hella Quality", "dEvrb6N70kA")
        .await;
    let first = until(|| w.navidrome.state.lock().scrobbles.first().cloned()).await;
    assert_eq!(
        (first.0.as_str(), first.1.as_str()),
        ("alice", local_id.as_str())
    );
}

#[tokio::test]
async fn play_queue_keeps_only_tracks_navidrome_knows() {
    need!("ffmpeg");
    let w = world().await;
    let local_id = w
        .download("deezer:1001", "Hella Quality", "dEvrb6N70kA")
        .await;
    w.navidrome.add_song(local_song("L1", "Local", "X", 100));
    w.json(
        "savePlayQueue",
        "id=L1&id=deezer:1001&id=deezer:1003&current=deezer:1001&position=5",
    )
    .await;
    let sent = w.requests("savePlayQueue").pop().unwrap();
    assert_eq!(all(&sent, "id"), ["L1", local_id.as_str()]);
    assert_eq!(
        (all(&sent, "current"), all(&sent, "position")),
        (vec![local_id.clone()], vec!["5".to_string()])
    );
    // The current track is not local yet: no current, no position.
    w.json(
        "savePlayQueue",
        "id=L1&id=deezer:1003&current=deezer:1003&position=5",
    )
    .await;
    let sent = w.requests("savePlayQueue").pop().unwrap();
    assert_eq!(all(&sent, "id"), ["L1"]);
    assert!(all(&sent, "current").is_empty() && all(&sent, "position").is_empty());
    let calls = w.navidrome.count("savePlayQueue");
    assert_eq!(
        w.json("savePlayQueue", "id=deezer:1003&current=deezer:1003")
            .await["status"],
        "ok"
    );
    assert_eq!(
        w.navidrome.count("savePlayQueue"),
        calls,
        "nothing local: not sent"
    );
    // By index: the index moves with dropped tracks.
    w.json(
        "savePlayQueueByIndex",
        "id=deezer:1003&id=L1&id=deezer:1001&currentIndex=2",
    )
    .await;
    let sent = w.requests("savePlayQueueByIndex").pop().unwrap();
    assert_eq!(
        (all(&sent, "id"), all(&sent, "currentIndex")),
        (vec!["L1".to_string(), local_id], vec!["1".to_string()])
    );
}

#[tokio::test]
async fn playlist_add_of_external_track() {
    need!("ffmpeg");
    let w = world().await;
    w.found("Third Song", "TTTTTTTTTTT");
    let root = w
        .json("createPlaylist", "name=Mix&songId=deezer:1003")
        .await;
    let playlist_id = root["playlist"]["id"].as_str().unwrap().to_string();
    let shown = &w.json("getPlaylist", &format!("id={playlist_id}")).await["playlist"]["entry"];
    assert_eq!(ids(shown), ["deezer:1003"]);
    let local_id = until(|| w.local_id("deezer:1003")).await;
    let songs = until(|| {
        w.navidrome
            .state
            .lock()
            .playlists
            .get(&playlist_id)
            .filter(|p| !p.is_empty())
            .cloned()
    })
    .await;
    assert_eq!(songs, [local_id]);
}

// ------------------------------------------------------------------ imports
async fn post_import(w: &World, body: Value) -> reqwest::Response {
    w.http
        .post(format!("{}/proxy/v1/imports", w.url))
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn wait_import(w: &World, id: &str, auth: &str) -> Value {
    for _ in 0..300 {
        let report: Value = w
            .http
            .get(format!("{}/proxy/v1/imports/{id}?{auth}", w.url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if report["status"] == "completed" {
            return report;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("import did not finish");
}

#[tokio::test]
async fn import_reports_are_private_and_like_can_be_off() {
    let w = world().await;
    let resp = post_import(
        &w,
        json!({"u": "alice", "p": "alicepw", "text": "TVETH - Hella Quality", "like": false}),
    )
    .await;
    assert_eq!(resp.status(), 202);
    let id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let report = wait_import(&w, &id, "u=alice&p=alicepw").await;
    assert_eq!(report["rows"][0]["track_id"], "deezer:1001");
    let other = w
        .http
        .get(format!("{}/proxy/v1/imports/{id}?u=bob&p=bobpw", w.url))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 404);
    let wrong = w
        .http
        .get(format!("{}/proxy/v1/imports/{id}?u=bob&p=wrong", w.url))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    assert!(
        w.s.store.pending_stars(Some("alice")).is_empty(),
        "like=false"
    );
    let list: Value = w
        .http
        .get(format!("{}/proxy/v1/imports?u=alice&p=alicepw", w.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list[0]["id"], json!(id));
}

#[tokio::test]
async fn import_accepts_token_auth_text_bodies_and_rejects_bad_input() {
    let w = world().await;
    let token = md5_token("alicepw", "c0ffee"); // "t=...&s=c0ffee"
    let t = token
        .trim_start_matches("t=")
        .split('&')
        .next()
        .unwrap()
        .to_string();
    let resp = post_import(
        &w,
        json!({"u": "alice", "t": t, "s": "c0ffee", "text": "TVETH - Hella Quality"}),
    )
    .await;
    assert_eq!(resp.status(), 202);
    let id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        wait_import(&w, &id, &format!("u=alice&{token}")).await["status"],
        "completed"
    );
    let plain = w
        .http
        .post(format!(
            "{}/proxy/v1/imports?u=alice&p=alicepw&like=no",
            w.url
        ))
        .header("Content-Type", "text/plain")
        .body("TVETH - Second Song")
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 202);
    let empty = w
        .http
        .post(format!("{}/proxy/v1/imports?u=alice&p=alicepw", w.url))
        .header("Content-Type", "text/plain")
        .body("\n")
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), 400);
    let broken = w
        .http
        .post(format!("{}/proxy/v1/imports", w.url))
        .header("Content-Type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(broken.status(), 400);
    let huge = w
        .http
        .post(format!("{}/proxy/v1/imports?u=alice&p=alicepw", w.url))
        .header("Content-Type", "text/plain")
        .body("A - B\n".repeat(200_000))
        .send()
        .await
        .unwrap();
    assert_eq!(huge.status(), 413);
}

#[tokio::test]
async fn import_page_is_served() {
    let w = world().await;
    let page = w
        .http
        .get(format!("{}/proxy/import", w.url))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.text().await.unwrap().contains("Import tracks"));
}

#[test]
fn method_names() {
    assert_eq!(method_name("getSong.view"), "getSong");
    assert_eq!(method_name("x/stream"), "stream");
}

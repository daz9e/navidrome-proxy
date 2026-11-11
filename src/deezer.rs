//! Deezer public API: the catalog behind every external track, album and artist.

use crate::db::Store;
use crate::util::{Json, arr, as_str, as_uint, obj, opt_str, truthy};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const API: &str = "https://api.deezer.com";
pub const DAY: f64 = 24.0 * 3600.0;

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct DeezerError(pub String);

pub type Result<T> = std::result::Result<T, DeezerError>;

tokio::task_local! {
    /// Set by background work (queue jobs, imports, reconciler) so a user's search
    /// never waits behind a bulk import for Deezer's shared request budget.
    pub static BACKGROUND: bool;
}

pub fn is_background() -> bool {
    BACKGROUND.try_with(|b| *b).unwrap_or(false)
}

/// A token bucket: Deezer allows 50 requests per 5 seconds per IP.
///
/// Background callers leave `reserve` tokens untouched for interactive ones.
pub struct RateLimiter {
    per_second: f64,
    capacity: f64,
    reserve: f64,
    state: Mutex<(f64, Instant)>,
    background: tokio::sync::Mutex<()>,
}

impl RateLimiter {
    pub fn new(per_second: f64, burst: u32, reserve: u32) -> Self {
        Self {
            per_second: per_second.max(0.1),
            capacity: burst as f64,
            reserve: reserve.min(burst.saturating_sub(1)) as f64,
            state: Mutex::new((burst as f64, Instant::now())),
            background: tokio::sync::Mutex::new(()),
        }
    }

    pub async fn acquire(&self, background: bool) {
        if !background {
            return self.take(1.0).await;
        }
        // Background callers queue among themselves, then wait for spare tokens.
        let _turn = self.background.lock().await;
        self.take(1.0 + self.reserve).await;
    }

    /// Wait until `needed` tokens are available, then consume one.
    async fn take(&self, needed: f64) {
        loop {
            let wait = {
                let mut state = self.state.lock();
                let now = Instant::now();
                state.0 = (state.0 + now.duration_since(state.1).as_secs_f64() * self.per_second).min(self.capacity);
                state.1 = now;
                if state.0 >= needed {
                    state.0 -= 1.0;
                    return;
                }
                (needed - state.0) / self.per_second
            };
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}

/// A small LRU with per-entry expiry.
pub struct Lru {
    size: usize,
    seq: u64,
    data: HashMap<String, (Instant, Value, u64)>,
    order: BTreeMap<u64, String>,
}

impl Lru {
    pub fn new(size: usize) -> Self {
        Self { size, seq: 0, data: HashMap::new(), order: BTreeMap::new() }
    }

    pub fn get(&mut self, key: &str) -> Option<Value> {
        let (expires, value, seq) = self.data.get(key)?.clone();
        self.order.remove(&seq);
        if expires < Instant::now() {
            self.data.remove(key);
            return None;
        }
        self.seq += 1;
        self.order.insert(self.seq, key.to_string());
        self.data.insert(key.to_string(), (expires, value.clone(), self.seq));
        Some(value)
    }

    pub fn put(&mut self, key: &str, value: Value, ttl: f64) {
        if let Some((_, _, seq)) = self.data.get(key) {
            self.order.remove(seq);
        }
        self.seq += 1;
        self.order.insert(self.seq, key.to_string());
        self.data.insert(key.to_string(), (Instant::now() + Duration::from_secs_f64(ttl.max(0.0)), value, self.seq));
        while self.data.len() > self.size {
            let Some((_, oldest)) = self.order.pop_first() else { break };
            self.data.remove(&oldest);
        }
    }
}

pub fn cover_url(md5: &str, size: u32, kind: &str) -> Option<String> {
    (!md5.is_empty())
        .then(|| format!("https://cdn-images.dzcdn.net/images/{kind}/{md5}/{size}x{size}-000000-80-0-0.jpg"))
}

pub fn best_cover(item: &Json) -> Option<String> {
    cover_url(&as_str(item.get("md5_image")), 1000, "cover")
        .or_else(|| opt_str(item.get("cover_xl")))
        .or_else(|| opt_str(item.get("cover_big")))
        .or_else(|| opt_str(item.get("cover_medium")))
        .or_else(|| opt_str(item.get("cover")))
}

/// All performers in credit order: main artist first, then contributors.
pub fn artist_names(track: &Json) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let primary = as_str(obj(track.get("artist")).get("name")).trim().to_string();
    if !primary.is_empty() {
        names.push(primary);
    }
    for contributor in arr(track.get("contributors")) {
        let name = as_str(contributor.get("name")).trim().to_string();
        if !name.is_empty() && !names.iter().any(|n| n.to_lowercase() == name.to_lowercase()) {
            names.push(name);
        }
    }
    names
}

pub fn year_of(date: &str) -> i64 {
    date.chars().take(4).collect::<String>().parse().unwrap_or(0)
}

fn non_zero(n: i64) -> Value {
    if n == 0 { Value::Null } else { json!(n) }
}

fn non_empty(s: String) -> Value {
    if s.is_empty() { Value::Null } else { Value::String(s) }
}

/// Normalize a Deezer track (search row or full object) into proxy metadata.
pub fn track_meta(track: &Json, album: Option<&Json>) -> Json {
    let empty = Json::new();
    let album = album.unwrap_or(&empty);
    let mut album_obj = obj(track.get("album"));
    album_obj.extend(album.clone());
    let artists = artist_names(track);
    let genres: Vec<String> =
        arr(obj(album.get("genres")).get("data")).iter().filter_map(|g| opt_str(g.get("name"))).collect();
    let album_artist = opt_str(obj(album.get("artist")).get("name"))
        .unwrap_or_else(|| artists.first().cloned().unwrap_or_default());
    let release = opt_str(album_obj.get("release_date")).or_else(|| opt_str(track.get("release_date"))).unwrap_or_default();
    let year = year_of(&release);
    let mut meta = json!({
        "source": "deezer",
        "deezer_id": as_str(track.get("id")),
        "title": as_str(track.get("title")),
        "title_version": as_str(track.get("title_version")),
        "artist": artists.join(", "),
        "artists": artists,
        "artist_id": as_str(obj(track.get("artist")).get("id")),
        "album": as_str(album_obj.get("title")),
        "album_id": non_empty(as_str(album_obj.get("id"))),
        "album_artist": album_artist,
        "duration": as_uint(track.get("duration")),
        "track": non_zero(as_uint(track.get("track_position"))),
        "disc": non_zero(as_uint(track.get("disk_number"))),
        "release_date": non_empty(release),
        "year": non_zero(year),
        "isrc": non_empty(as_str(track.get("isrc"))),
        "explicit": truthy(track.get("explicit_lyrics")),
        "cover": best_cover(&album_obj).or_else(|| best_cover(track)),
        "cover_md5": opt_str(album_obj.get("md5_image")).or_else(|| opt_str(track.get("md5_image"))),
        "genre": genres.first(),
        "track_total": non_zero(as_uint(album.get("nb_tracks"))),
        "rank": as_uint(track.get("rank")),
        // Search rows carry only the main artist; a full object carries contributors.
        "full": track.contains_key("contributors"),
    });
    std::mem::take(meta.as_object_mut().unwrap())
}

fn data_list(v: &Value) -> Vec<Json> {
    arr(v.get("data")).into_iter().filter_map(|x| x.as_object().cloned()).collect()
}

pub fn quote(query: &str) -> String {
    percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
        // Python's quote(safe="") keeps these unreserved characters.
        .replace("%2D", "-")
        .replace("%2E", ".")
        .replace("%5F", "_")
        .replace("%7E", "~")
}

pub struct Deezer {
    store: Arc<Store>,
    base_url: String,
    pub limiter: RateLimiter,
    search_ttl: f64,
    memory: Mutex<Lru>,
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    http: reqwest::Client,
    /// Backoff step between retries (0.5 s; quota errors wait three steps).
    pub retry_unit: Duration,
}

impl Deezer {
    pub fn new(store: Arc<Store>, per_second: f64, base_url: &str, search_ttl: f64) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            .user_agent("navidrome-proxy/3")
            .build()
            .expect("http client");
        Self {
            store,
            base_url: base_url.trim_end_matches('/').to_string(),
            limiter: RateLimiter::new(per_second, 10, 4),
            search_ttl,
            memory: Mutex::new(Lru::new(4096)),
            inflight: Mutex::new(HashMap::new()),
            http,
            retry_unit: Duration::from_millis(500),
        }
    }

    pub fn clear_memory(&self) {
        *self.memory.lock() = Lru::new(4096);
    }

    fn cached(&self, key: &str, ttl: f64, persist: bool) -> Option<Value> {
        if let Some(v) = self.memory.lock().get(key) {
            return Some(v);
        }
        if persist && let Some(stored) = self.store.cache_get(key) {
            self.memory.lock().put(key, stored.clone(), ttl.min(3600.0));
            return Some(stored);
        }
        None
    }

    /// GET `path` with memory (and optionally SQLite) caching and single-flight.
    pub async fn get(&self, path: &str, ttl: f64, persist: bool) -> Result<Value> {
        let key = format!("deezer:{path}");
        if let Some(v) = self.cached(&key, ttl, persist) {
            return Ok(v);
        }
        let gate = self.inflight.lock().entry(key.clone()).or_default().clone();
        let _flight = gate.lock().await;
        // Whoever held the gate before us may have filled the cache.
        if let Some(v) = self.cached(&key, ttl, persist) {
            return Ok(v);
        }
        let result = self.fetch(path).await;
        if let Ok(data) = &result {
            self.memory.lock().put(&key, data.clone(), ttl);
            if persist {
                self.store.cache_set(&key, data, ttl);
            }
        }
        {
            let mut inflight = self.inflight.lock();
            // Only the last user of the gate removes it.
            if Arc::strong_count(&gate) <= 2 {
                inflight.remove(&key);
            }
        }
        result
    }

    async fn fetch(&self, path: &str) -> Result<Value> {
        let url = format!("{}/{}", self.base_url, path.trim_start_matches('/'));
        let mut last = String::new();
        for attempt in 0..4u32 {
            self.limiter.acquire(is_background()).await;
            let response = match self.http.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = e.to_string();
                    tokio::time::sleep(self.retry_unit * (attempt + 1)).await;
                    continue;
                }
            };
            let status = response.status().as_u16();
            let body = if status >= 500 || status == 429 {
                Err(format!("HTTP {status}"))
            } else {
                match response.bytes().await {
                    Ok(b) => serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            };
            let data = match body {
                Ok(Value::Object(m)) => m,
                Ok(_) => return Err(DeezerError(format!("{path}: unexpected response"))),
                Err(e) => {
                    last = e;
                    tokio::time::sleep(self.retry_unit * (attempt + 1)).await;
                    continue;
                }
            };
            if let Some(error) = data.get("error").filter(|e| truthy(Some(e))) {
                match error.get("code").and_then(Value::as_i64) {
                    Some(4) => {
                        // Quota limit exceeded.
                        last = "quota exceeded".into();
                        tokio::time::sleep(self.retry_unit * 3 * (attempt + 1)).await;
                        continue;
                    }
                    Some(800) => return Err(DeezerError(format!("not found: {path}"))),
                    _ => {
                        return Err(DeezerError(format!(
                            "{}: {}",
                            as_str(error.get("type")),
                            as_str(error.get("message"))
                        )));
                    }
                }
            }
            return Ok(Value::Object(data));
        }
        Err(DeezerError(format!("{path}: {last}")))
    }

    // --------------------------------------------------------------- search
    pub async fn search_tracks(&self, query: &str, limit: usize, index: usize) -> Result<Vec<Json>> {
        let data =
            self.get(&format!("search/track?q={}&limit={limit}&index={index}", quote(query)), self.search_ttl, false).await?;
        Ok(data_list(&data)
            .into_iter()
            .filter(|x| {
                x.get("type").and_then(Value::as_str).unwrap_or("track") == "track"
                    && x.get("readable").map(|r| truthy(Some(r))).unwrap_or(true)
            })
            .collect())
    }

    pub async fn search_albums(&self, query: &str, limit: usize) -> Result<Vec<Json>> {
        let data = self.get(&format!("search/album?q={}&limit={limit}", quote(query)), self.search_ttl, false).await?;
        Ok(data_list(&data))
    }

    pub async fn search_artists(&self, query: &str, limit: usize) -> Result<Vec<Json>> {
        let data = self.get(&format!("search/artist?q={}&limit={limit}", quote(query)), self.search_ttl, false).await?;
        Ok(data_list(&data))
    }

    // ---------------------------------------------------------------- lookup
    pub async fn track(&self, id: &str) -> Result<Json> {
        Ok(obj(Some(&self.get(&format!("track/{id}"), 7.0 * DAY, true).await?)))
    }

    pub async fn album(&self, id: &str) -> Result<Json> {
        Ok(obj(Some(&self.get(&format!("album/{id}"), 7.0 * DAY, true).await?)))
    }

    pub async fn album_tracks(&self, id: &str) -> Result<Vec<Json>> {
        Ok(data_list(&self.get(&format!("album/{id}/tracks?limit=500"), 7.0 * DAY, true).await?))
    }

    /// Album tracks only if already cached; never hits the network.
    pub fn cached_album_tracks(&self, id: &str) -> Option<Vec<Json>> {
        let key = format!("deezer:album/{id}/tracks?limit=500");
        let data = self.memory.lock().get(&key).or_else(|| self.store.cache_get(&key))?;
        Some(data_list(&data))
    }

    pub fn cached_album(&self, id: &str) -> Option<Json> {
        let key = format!("deezer:album/{id}");
        let data = self.memory.lock().get(&key).or_else(|| self.store.cache_get(&key))?;
        data.as_object().cloned()
    }

    pub async fn artist(&self, id: &str) -> Result<Json> {
        Ok(obj(Some(&self.get(&format!("artist/{id}"), DAY, true).await?)))
    }

    pub async fn artist_albums(&self, id: &str) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        for index in (0..500).step_by(100) {
            let data = self.get(&format!("artist/{id}/albums?limit=100&index={index}"), DAY, true).await?;
            let page = data_list(&data);
            let done = page.len() < 100 || !truthy(data.get("next"));
            out.extend(page);
            if done {
                break;
            }
        }
        Ok(out)
    }

    pub async fn artist_top(&self, id: &str, limit: usize) -> Result<Vec<Json>> {
        Ok(data_list(&self.get(&format!("artist/{id}/top?limit={limit}"), DAY, true).await?))
    }

    pub async fn artist_related(&self, id: &str, limit: usize) -> Result<Vec<Json>> {
        Ok(data_list(&self.get(&format!("artist/{id}/related?limit={limit}"), DAY, true).await?))
    }

    /// Complete metadata: full track (contributors, ISRC) plus its album (genre, totals).
    pub async fn full_track_meta(&self, track_id: &str) -> Result<Json> {
        let track = self.track(track_id).await?;
        let album_id = as_str(obj(track.get("album")).get("id"));
        let mut album = None;
        if !album_id.is_empty() {
            match self.album(&album_id).await {
                Ok(a) => album = Some(a),
                Err(e) => tracing::info!("album {album_id} unavailable for track {track_id}: {e}"),
            }
        }
        Ok(track_meta(&track, album.as_ref()))
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::testutil::{FakeDeezer, dz_track, serve};

    #[test]
    fn track_meta_keeps_every_contributor() {
        let t = dz_track(1, "Hella Quality", "TVETH", 100, "RAW", 1, &["TVETH", "Bato"]);
        let album = json!({"nb_tracks": 12, "genres": {"data": [{"name": "Rap"}]}, "artist": {"name": "TVETH"}});
        let meta = track_meta(&t, album.as_object());
        assert_eq!(meta["artists"], json!(["TVETH", "Bato"]));
        assert_eq!(meta["artist"], "TVETH, Bato");
        assert_eq!(meta["album_artist"], "TVETH");
        assert_eq!((meta["year"].clone(), meta["genre"].clone(), meta["track_total"].clone()), (json!(2020), json!("Rap"), json!(12)));
        assert!(meta["cover"].as_str().unwrap().ends_with("/md5100/1000x1000-000000-80-0-0.jpg"));
        assert_eq!(meta["full"], true);
        assert_eq!(meta["album_id"], "100");
        assert_eq!(track_meta(&dz_track(1, "x", "y", 100, "RAW", 1, &[]), None)["full"], false);
    }

    async fn client(fake: &FakeDeezer) -> (Deezer, crate::testutil::Server) {
        let server = serve(fake.router()).await;
        let store = Arc::new(Store::memory());
        let mut dz = Deezer::new(store, 100.0, &server.url, 120.0);
        dz.retry_unit = Duration::from_millis(1);
        (dz, server)
    }

    #[tokio::test]
    async fn search_and_full_track_meta() {
        let fake = FakeDeezer::default();
        let (dz, _s) = client(&fake).await;
        let rows = dz.search_tracks("hella quality", 25, 0).await.unwrap();
        assert_eq!(rows.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(), vec![json!(4250555171u64)]);
        let meta = dz.full_track_meta("4250555171").await.unwrap();
        assert_eq!(meta["artists"], json!(["TVETH", "Bato"]));
        assert_eq!(meta["genre"], "Rap");
    }

    #[tokio::test]
    async fn single_flight_and_persistent_cache() {
        let fake = FakeDeezer::default();
        let (dz, _s) = client(&fake).await;
        let dz = Arc::new(dz);
        let calls: Vec<_> = (0..5).map(|_| { let d = dz.clone(); tokio::spawn(async move { d.album("100").await }) }).collect();
        for c in calls {
            c.await.unwrap().unwrap();
        }
        assert_eq!(fake.hits("/album/100"), 1);
        dz.clear_memory();
        dz.album("100").await.unwrap();
        assert_eq!(fake.hits("/album/100"), 1, "served from SQLite");
        assert!(dz.cached_album_tracks("100").is_none());
        dz.album_tracks("100").await.unwrap();
        let ids: Vec<_> = dz.cached_album_tracks("100").unwrap().iter().map(|t| t["id"].clone()).collect();
        assert_eq!(ids, vec![json!(4250555171u64), json!(2)]);
    }

    #[tokio::test]
    async fn quota_error_is_retried_and_not_found_raises() {
        let fake = FakeDeezer::default();
        fake.state.lock().quota_failures = 2;
        let (dz, _s) = client(&fake).await;
        assert_eq!(dz.artist("1").await.unwrap()["name"], "TVETH");
        assert!(dz.track("999").await.unwrap_err().0.contains("not found"));
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limiter_spaces_requests() {
        let limiter = RateLimiter::new(20.0, 2, 4);
        let start = tokio::time::Instant::now();
        for _ in 0..6 {
            limiter.acquire(false).await;
        }
        assert!(start.elapsed() >= Duration::from_millis(180));
    }

    #[tokio::test(start_paused = true)]
    async fn background_callers_leave_a_reserve_for_interactive_ones() {
        let limiter = Arc::new(RateLimiter::new(5.0, 6, 4));
        let order = Arc::new(Mutex::new(Vec::new()));
        for i in 0..4 {
            let (l, o) = (limiter.clone(), order.clone());
            tokio::spawn(async move {
                l.acquire(true).await;
                o.lock().push(format!("bg{i}"));
            });
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        tokio::time::timeout(Duration::from_millis(1), limiter.acquire(false)).await.expect("served from the reserve");
        order.lock().push("fg".into());
        let order = order.lock().clone();
        assert_eq!(&order[..2], ["bg0", "bg1"]);
        assert!(order[..3].contains(&"fg".to_string()));
    }

    #[test]
    fn lru_evicts_oldest_and_expires() {
        let mut lru = Lru::new(2);
        lru.put("a", json!(1), 60.0);
        lru.put("b", json!(2), 60.0);
        lru.get("a");
        lru.put("c", json!(3), 60.0);
        assert!(lru.get("b").is_none());
        assert!(lru.get("a").is_some());
        lru.put("d", json!(4), 0.0);
        assert!(lru.get("d").is_none());
    }

    #[test]
    fn quote_matches_python() {
        assert_eq!(quote("a b/c-d.e_f~ё"), "a%20b%2Fc-d.e_f~%D1%91");
    }
}

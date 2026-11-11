//! Talking to the upstream Navidrome server.

use crate::subsonic::params::{Params, redact};
use crate::subsonic::response::ServerIdentity;
use crate::util::{Json, as_str};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

#[derive(Debug, Clone, thiserror::Error)]
pub enum NdError {
    /// Navidrome answered with `status="failed"`.
    #[error("{code}: {message}")]
    Upstream { code: i64, message: String },
    /// Navidrome could not be reached.
    #[error("Navidrome unavailable: {0}")]
    Unavailable(String),
}

pub type Result<T> = std::result::Result<T, NdError>;

#[derive(Debug, Clone, PartialEq)]
pub struct AuthResult {
    pub ok: bool,
    pub unavailable: bool,
    pub code: i64,
    pub message: String,
}

impl AuthResult {
    fn ok() -> Self {
        Self { ok: true, unavailable: false, code: 0, message: String::new() }
    }

    fn failed(code: i64, message: &str) -> Self {
        Self { ok: false, unavailable: false, code, message: message.into() }
    }
}

/// What a relayed request carries besides its body.
pub struct Incoming<'a> {
    pub method: &'a Method,
    pub path_and_query: &'a str,
    pub headers: &'a HeaderMap,
    pub remote: Option<std::net::IpAddr>,
}

pub struct Navidrome {
    pub base_url: String,
    pub identity: RwLock<ServerIdentity>,
    auth_ttl: Duration,
    auth_cache: Mutex<HashMap<Vec<String>, (Instant, AuthResult)>>,
    /// Latest credentials that Navidrome accepted for each user. Kept in memory
    /// only: they let background work (syncing a like once a download finishes)
    /// act as that user without ever writing secrets to disk.
    user_credentials: RwLock<HashMap<String, Params>>,
    api: reqwest::Client,
    raw: reqwest::Client,
}

impl Navidrome {
    pub fn new(base_url: &str, auth_ttl: f64) -> Self {
        let build = |pool: usize| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .read_timeout(Duration::from_secs(120))
                .pool_max_idle_per_host(pool)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("http client")
        };
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            identity: RwLock::new(ServerIdentity::default()),
            auth_ttl: Duration::from_secs_f64(auth_ttl.max(0.0)),
            auth_cache: Mutex::new(HashMap::new()),
            user_credentials: RwLock::new(HashMap::new()),
            api: build(64),
            raw: build(128),
        }
    }

    pub fn identity(&self) -> ServerIdentity {
        self.identity.read().clone()
    }

    // ------------------------------------------------------------ JSON calls
    /// Call `method` and return the `subsonic-response` object.
    pub async fn call(&self, method: &str, params: &Params, extra: &[(&str, &str)]) -> Result<Json> {
        self.call_many(method, params, &[], extra).await
    }

    /// Like `call` but with repeated parameters (`id=1&id=2`).
    pub async fn call_many(
        &self,
        method: &str,
        params: &Params,
        repeated: &[(&str, Vec<String>)],
        extra: &[(&str, &str)],
    ) -> Result<Json> {
        let mut query = params.credentials().with("f", "json");
        for (k, v) in extra {
            query = query.with(k, *v);
        }
        for (k, values) in repeated {
            query = query.replace(k, values.iter().cloned());
        }
        self.get(method, query, Duration::from_secs(30)).await
    }

    async fn get(&self, method: &str, mut query: Params, timeout: Duration) -> Result<Json> {
        if !query.contains("c") {
            query = query.with("c", "navidrome-proxy");
        }
        if !query.contains("v") {
            query = query.with("v", "1.16.1");
        }
        let url = format!("{}/rest/{method}?{}", self.base_url, query.encode());
        let response = self.api.get(&url).timeout(timeout).send().await.map_err(|e| NdError::Unavailable(format!("{method}: {e}")))?;
        let status = response.status();
        let raw = response.bytes().await.map_err(|e| NdError::Unavailable(format!("{method}: {e}")))?;
        let root = serde_json::from_slice::<Value>(&raw)
            .ok()
            .and_then(|v| v.get("subsonic-response").and_then(Value::as_object).cloned())
            .ok_or_else(|| NdError::Unavailable(format!("{method}: HTTP {status}, unexpected response")))?;
        if root.get("status").and_then(Value::as_str) != Some("ok") {
            let error = crate::util::obj(root.get("error"));
            return Err(NdError::Upstream {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: crate::util::opt_str(error.get("message")).unwrap_or_else(|| "failed".into()),
            });
        }
        Ok(root)
    }

    // ------------------------------------------------------------------ auth
    fn auth_key(params: &Params) -> Vec<String> {
        ["u", "p", "t", "s", "apiKey"].iter().map(|k| params.get(k).to_string()).collect()
    }

    /// Validate the caller's credentials against Navidrome (cached).
    pub async fn authenticate(&self, params: &Params) -> AuthResult {
        if !params.has_credentials() {
            return AuthResult::failed(10, "Required parameter is missing");
        }
        let key = Self::auth_key(params);
        let now = Instant::now();
        if let Some((expires, result)) = self.auth_cache.lock().get(&key).cloned()
            && expires > now
        {
            if result.ok {
                self.remember(params);
            }
            return result;
        }
        let root = match self.get("ping", params.credentials(), Duration::from_secs(10)).await {
            Ok(root) => root,
            Err(NdError::Upstream { code, message }) => {
                let result = AuthResult::failed(code, &message);
                self.auth_cache.lock().insert(key, (now + Duration::from_secs(15), result.clone()));
                return result;
            }
            Err(NdError::Unavailable(e)) => {
                tracing::warn!("auth check failed, Navidrome unavailable: {e}");
                return AuthResult { unavailable: true, ..AuthResult::failed(0, "Navidrome is unavailable") };
            }
        };
        self.identity.write().update_from(&root);
        let mut cache = self.auth_cache.lock();
        if cache.len() > 10_000 {
            cache.clear();
        }
        cache.insert(key, (now + self.auth_ttl, AuthResult::ok()));
        drop(cache);
        self.remember(params);
        AuthResult::ok()
    }

    fn remember(&self, params: &Params) {
        self.user_credentials.write().insert(params.username().to_string(), params.credentials());
    }

    pub fn credentials_for(&self, username: &str) -> Option<Params> {
        self.user_credentials.read().get(username).cloned()
    }

    pub fn any_credentials(&self) -> Option<Params> {
        self.user_credentials.read().values().next().cloned()
    }

    /// Learn the upstream version strings (an unauthenticated ping still carries them).
    pub async fn refresh_identity(&self) {
        let url = format!("{}/rest/ping?f=json", self.base_url);
        let result = async {
            let v: Value = self.api.get(&url).timeout(Duration::from_secs(10)).send().await?.json().await?;
            Ok::<_, reqwest::Error>(v)
        }
        .await;
        match result {
            Ok(v) => {
                if let Some(root) = v.get("subsonic-response").and_then(Value::as_object) {
                    self.identity.write().update_from(root);
                }
            }
            Err(e) => tracing::warn!("could not read Navidrome identity: {e}"),
        }
    }

    // ----------------------------------------------------------- passthrough
    /// Relay a request to Navidrome byte-for-byte, streaming the response.
    pub async fn passthrough(&self, req: Incoming<'_>, body: reqwest::Body) -> Result<Response> {
        let target = format!("{}{}", self.base_url, req.path_and_query);
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in req.headers {
            if !HOP_BY_HOP.contains(&k.as_str()) {
                headers.append(k.clone(), v.clone());
            }
        }
        if let Some(host) = req.headers.get("host") {
            headers.insert("host", host.clone());
            headers.insert("x-forwarded-host", host.clone());
        }
        // Never ask for a compressed body on the client's behalf.
        if !headers.contains_key("accept-encoding") {
            headers.insert("accept-encoding", HeaderValue::from_static("identity"));
        }
        if !headers.contains_key("x-forwarded-proto") {
            headers.insert("x-forwarded-proto", HeaderValue::from_static("http"));
        }
        if let Some(remote) = req.remote {
            let value = match req.headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
                Some(prior) => format!("{prior}, {remote}"),
                None => remote.to_string(),
            };
            if let Ok(v) = HeaderValue::from_str(&value) {
                headers.insert("x-forwarded-for", v);
            }
        }
        let upstream = self
            .raw
            .request(req.method.clone(), &target)
            .headers(headers)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!("passthrough {} failed: {e}", redact(&target));
                NdError::Unavailable(e.to_string())
            })?;
        let mut builder = Response::builder().status(StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
        for (k, v) in upstream.headers() {
            if !HOP_BY_HOP.contains(&k.as_str()) {
                builder = builder.header(k, v);
            }
        }
        // A malformed upstream length is dropped and the body goes out chunked.
        if let Some(len) = upstream.headers().get("content-length").and_then(|v| v.to_str().ok()).filter(|v| v.parse::<u64>().is_ok()) {
            builder = builder.header(HeaderName::from_static("content-length"), len);
        }
        let body = if req.method == Method::HEAD { Body::empty() } else { Body::from_stream(upstream.bytes_stream()) };
        builder.body(body).map_err(|e| NdError::Unavailable(e.to_string()))
    }
}

/// The `song` list of a Subsonic container (`{"song": [...]}`), tolerating a missing key.
pub fn songs_of(container: Option<&Value>) -> Vec<Json> {
    crate::util::arr(container.and_then(|c| c.get("song")))
        .into_iter()
        .filter_map(|s| s.as_object().cloned())
        .collect()
}

pub fn id_of(item: &Json) -> String {
    as_str(item.get("id"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeNavidrome, md5_token, serve};

    async fn upstream() -> (FakeNavidrome, Navidrome, crate::testutil::Server) {
        let fake = FakeNavidrome::default();
        fake.add_song(crate::testutil::nd_song("L1", "Starboy", "The Weeknd", "Album", "al1"));
        let server = serve(fake.router()).await;
        let nav = Navidrome::new(&server.url, 60.0);
        (fake, nav, server)
    }

    #[tokio::test]
    async fn authenticate_caches_and_remembers_credentials() {
        let (fake, nav, _s) = upstream().await;
        let params = Params::parse(&format!("u=alice&{}&v=1.16.1&c=test&f=json", md5_token("alicepw", "abc")), "");
        assert!(nav.authenticate(&params).await.ok);
        assert!(nav.authenticate(&params).await.ok);
        assert_eq!(fake.count("ping"), 1);
        assert!(!nav.credentials_for("alice").unwrap().get("t").is_empty());
        assert_eq!(nav.identity().server_version, "0.63.2 (fake)");
    }

    #[tokio::test]
    async fn authenticate_rejects_wrong_and_missing() {
        let (_f, nav, _s) = upstream().await;
        let bad = nav.authenticate(&Params::parse("u=alice&p=nope", "")).await;
        assert!(!bad.ok && bad.code == 40);
        let missing = nav.authenticate(&Params::parse("u=alice", "")).await;
        assert!(!missing.ok && missing.code == 10);
        assert!(nav.credentials_for("alice").is_none());
    }

    #[tokio::test]
    async fn call_returns_root_or_raises() {
        let (_f, nav, _s) = upstream().await;
        let params = Params::parse("u=bob&p=bobpw", "");
        let root = nav.call("getSong", &params, &[("id", "L1")]).await.unwrap();
        assert_eq!(root["song"]["title"], "Starboy");
        match nav.call("getSong", &params, &[("id", "missing")]).await {
            Err(NdError::Upstream { code, .. }) => assert_eq!(code, 70),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn unavailable_upstream() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let nav = Navidrome::new(&url, 60.0);
        assert!(matches!(nav.call("ping", &Params::parse("u=a&p=b", ""), &[]).await, Err(NdError::Unavailable(_))));
        let result = nav.authenticate(&Params::parse("u=a&p=b", "")).await;
        assert!(result.unavailable && !result.ok);
    }

    #[tokio::test]
    async fn passthrough_streams_range_and_forwards_host() {
        let (_f, nav, _s) = upstream().await;
        let nav = std::sync::Arc::new(nav);
        let relay = axum::Router::new().fallback({
            let nav = nav.clone();
            move |req: axum::extract::Request| {
                let nav = nav.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let pq = parts.uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
                    let incoming = Incoming { method: &parts.method, path_and_query: &pq, headers: &parts.headers, remote: None };
                    nav.passthrough(incoming, reqwest::Body::wrap_stream(body.into_data_stream())).await.unwrap()
                }
            }
        });
        let proxy = serve(relay).await;
        let http = reqwest::Client::new();
        let resp = http
            .get(format!("{}/rest/stream.view?u=alice&p=alicepw&id=L1", proxy.url))
            .header("Range", "bytes=10-19")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.headers()["content-range"], "bytes 10-19/50000");
        assert_eq!(resp.headers()["content-length"], "10");
        assert_eq!(resp.bytes().await.unwrap().len(), 10);
        let ui = http.get(format!("{}/app/", proxy.url)).header("Host", "music.example.org").send().await.unwrap();
        assert_eq!(ui.headers()["x-seen-host"], "music.example.org");
        assert_eq!(ui.headers()["x-seen-encoding"], "identity");
        assert_eq!(ui.text().await.unwrap(), "navidrome ui /app/");
        let gz = http.get(format!("{}/app/", proxy.url)).header("Accept-Encoding", "gzip").send().await.unwrap();
        assert_eq!(gz.headers()["x-seen-encoding"], "gzip");
        let head = http.head(format!("{}/rest/stream.view?u=alice&p=alicepw&id=L1", proxy.url)).send().await.unwrap();
        assert_eq!(head.headers()["content-length"], "50000");
    }
}

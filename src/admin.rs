//! The proxy's own HTTP API under `/proxy/v1` and the import page.
//!
//! `Response` errors are large, but that is the type axum handlers return anyway.
#![allow(clippy::result_large_err)]
//!
//! Authentication uses the caller's Navidrome account, given as Subsonic parameters
//! in the query string (`u` + `p`/`t`/`s`) or as `u`/`p` fields of a JSON body.

use crate::app::Services;
use crate::subsonic::params::Params;
use crate::util::{Json, as_str};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};
use std::sync::Arc;

const IMPORT_PAGE: &str = include_str!("../static/import.html");
const MAX_LIST_BYTES: usize = 1024 * 1024;

type S = State<Arc<Services>>;

fn reply(status: StatusCode, body: Value) -> Response {
    (status, axum::Json(body)).into_response()
}

/// Parsed JSON/form body (or {}) and the raw text body for text/plain posts.
fn payload(headers: &HeaderMap, body: &Bytes) -> Result<(Json, String), Response> {
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let media = ctype.split(';').next().unwrap_or("").trim().to_lowercase();
    let raw = String::from_utf8_lossy(body).into_owned();
    match media.as_str() {
        "application/json" => {
            let text = if raw.trim().is_empty() {
                "{}"
            } else {
                raw.as_str()
            };
            match serde_json::from_str::<Value>(text) {
                Ok(Value::Object(m)) => Ok((m, String::new())),
                Ok(_) => Ok((Json::new(), String::new())),
                Err(e) => Err(reply(
                    StatusCode::BAD_REQUEST,
                    json!({"error": format!("invalid JSON: {e}")}),
                )),
            }
        }
        "application/x-www-form-urlencoded" => {
            let form: Json = Params::parse(&raw, "")
                .items()
                .iter()
                .map(|(k, v)| (k.clone(), json!(v)))
                .collect();
            Ok((form, String::new()))
        }
        _ => Ok((Json::new(), raw)),
    }
}

/// `str(value)` of a JSON value, Python style for booleans.
fn text(v: &Value) -> String {
    match v {
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => as_str(Some(other)),
    }
}

async fn authenticate(s: &Services, query: &str, payload: &Json) -> Result<Params, Response> {
    let mut params = Params::parse(query, "");
    if !params.has_credentials() && payload.get("u").is_some_and(|u| !text(u).is_empty()) {
        params = Params::new(["u", "p", "t", "s"].iter().filter_map(|k| {
            payload
                .get(*k)
                .map(text)
                .filter(|v| !v.is_empty())
                .map(|v| (*k, v))
        }));
    }
    let result = s.navidrome.authenticate(&params).await;
    if result.ok {
        return Ok(params);
    }
    let status = if result.unavailable {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::UNAUTHORIZED
    };
    Err(reply(status, json!({"error": result.message})))
}

async fn health() -> Response {
    reply(StatusCode::OK, json!({"ok": true}))
}

async fn status(State(s): S, RawQuery(q): RawQuery) -> Response {
    let auth = match authenticate(&s, q.as_deref().unwrap_or(""), &Json::new()).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let user = auth.username();
    let running: Vec<String> = s
        .scheduler
        .running()
        .into_iter()
        .map(|(track, kind)| format!("{kind}:{track}"))
        .collect();
    reply(
        StatusCode::OK,
        json!({
            "uptime_seconds": (crate::util::now() - s.started).round() as i64,
            "tracks": s.store.status_counts(),
            "jobs": s.store.job_counts(),
            "running": running,
            "pending": {
                "stars": s.store.pending_stars(Some(user)).len(),
                "scrobbles": s.store.pending_scrobbles(user).len(),
                "playlist_adds": s.store.pending_playlist_adds(user).len(),
            },
        }),
    )
}

async fn create_import(
    State(s): S,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (payload, raw) = match payload(&headers, &body) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let q = q.unwrap_or_default();
    let auth = match authenticate(&s, &q, &payload).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let list = payload
        .get("text")
        .map(text)
        .filter(|t| !t.is_empty())
        .unwrap_or(raw);
    if list.len() > MAX_LIST_BYTES {
        return reply(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({"error": "list is larger than 1 MiB"}),
        );
    }
    let query = Params::parse(&q, "");
    let like = payload
        .get("like")
        .map(text)
        .unwrap_or_else(|| query.get_or("like", "true").to_string());
    let like = !matches!(like.to_lowercase().as_str(), "false" | "0" | "no");
    let playlist = payload
        .get("playlist")
        .map(text)
        .filter(|p| !p.is_empty())
        .or_else(|| Some(query.get("playlist").to_string()).filter(|p| !p.is_empty()));
    match s
        .importer
        .create(auth.username(), &list, like, playlist.as_deref())
    {
        Ok(report) => reply(
            StatusCode::ACCEPTED,
            json!({"id": report["id"], "status": report["status"], "progress": report["progress"]}),
        ),
        Err(e) => reply(StatusCode::BAD_REQUEST, json!({"error": e})),
    }
}

async fn get_import(State(s): S, Path(id): Path<String>, RawQuery(q): RawQuery) -> Response {
    let auth = match authenticate(&s, q.as_deref().unwrap_or(""), &Json::new()).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    match s
        .store
        .get_import(&id)
        .filter(|imp| as_str(imp.get("username")) == auth.username())
        .and_then(|_| s.importer.report(&id))
    {
        Some(report) => reply(StatusCode::OK, Value::Object(report)),
        None => reply(StatusCode::NOT_FOUND, json!({"error": "not found"})),
    }
}

async fn list_imports(State(s): S, RawQuery(q): RawQuery) -> Response {
    let auth = match authenticate(&s, q.as_deref().unwrap_or(""), &Json::new()).await {
        Ok(a) => a,
        Err(r) => return r,
    };
    let reports: Vec<Value> = s
        .store
        .list_imports(auth.username(), 20)
        .iter()
        .filter_map(|imp| s.importer.report(&as_str(imp.get("id"))))
        .map(|r| {
            let keep: Json = [
                "id",
                "status",
                "created_at",
                "progress",
                "counts",
                "options",
            ]
            .iter()
            .map(|k| (k.to_string(), r.get(*k).cloned().unwrap_or(Value::Null)))
            .collect();
            Value::Object(keep)
        })
        .collect();
    reply(StatusCode::OK, Value::Array(reports))
}

async fn import_page() -> Html<&'static str> {
    Html(IMPORT_PAGE)
}

pub fn routes() -> Router<Arc<Services>> {
    Router::new()
        .route("/proxy/v1/health", get(health))
        .route("/proxy/v1/status", get(status))
        .route("/proxy/v1/imports", get(list_imports).post(create_import))
        .route("/proxy/v1/imports/{id}", get(get_import))
        .route("/proxy/import", get(import_page))
}

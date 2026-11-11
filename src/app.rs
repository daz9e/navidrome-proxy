//! The HTTP application: wiring, routing and lifecycle.

use crate::api::{self, Call};
use crate::audio::Tools;
use crate::catalog::Catalog;
use crate::config::Settings;
use crate::covers::Covers;
use crate::db::{Store, Track};
use crate::deezer::Deezer;
use crate::identity::Identity;
use crate::imports::Importer;
use crate::intents::Intents;
use crate::library::{Indexer, Library};
use crate::navidrome::{Incoming, Navidrome, NdError};
use crate::pipeline::{Fetcher, Pipeline};
use crate::reconcile::Reconciler;
use crate::scheduler::{BACKGROUND, Scheduler};
use crate::slskd::{Slskd, Upgrader};
use crate::sources::{Backend, Sources, YtBackend};
use crate::subsonic::params::{Params, redact};
use crate::subsonic::response as sr;
use crate::tasks::Background;
use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MAX_BODY: usize = 4 * 1024 * 1024;

pub struct Services {
    pub settings: Arc<Settings>,
    pub store: Arc<Store>,
    pub identity: Identity,
    pub navidrome: Arc<Navidrome>,
    pub deezer: Arc<Deezer>,
    pub finder: Arc<Sources>,
    pub library: Arc<Library>,
    pub indexer: Arc<Indexer>,
    pub covers: Arc<Covers>,
    pub pipeline: Arc<Pipeline>,
    pub scheduler: Arc<Scheduler>,
    pub catalog: Arc<Catalog>,
    pub intents: Arc<Intents>,
    pub importer: Arc<Importer>,
    pub reconciler: Arc<Reconciler>,
    pub upgrader: Option<Arc<Upgrader>>,
    pub background: Arc<Background>,
    pub search_pages: Mutex<api::search::PageMemory>,
    pub started: f64,
    backups: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Replaceable upstreams (tests point them at fakes).
pub struct Overrides {
    pub store: Option<Arc<Store>>,
    pub deezer_url: String,
    pub lyrics_url: String,
    pub backend: Option<Arc<dyn Backend>>,
    pub fetcher: Option<Arc<dyn Fetcher>>,
}

impl Default for Overrides {
    fn default() -> Self {
        Self {
            store: None,
            deezer_url: crate::deezer::API.into(),
            lyrics_url: crate::lyrics::LRCLIB.into(),
            backend: None,
            fetcher: None,
        }
    }
}

pub fn build_services(settings: Settings, o: Overrides) -> anyhow::Result<Arc<Services>> {
    let settings = Arc::new(settings);
    let store = match o.store {
        Some(store) => store,
        None => Arc::new(Store::open(&settings.db_path())?),
    };
    let identity = Identity::new(store.clone());
    let navidrome = Arc::new(Navidrome::new(
        &settings.navidrome_url,
        settings.auth_cache_seconds,
    ));
    let deezer = Arc::new(Deezer::new(
        store.clone(),
        settings.deezer_requests_per_second,
        &o.deezer_url,
        settings.search_cache_seconds,
    ));
    let backend = o.backend.unwrap_or_else(|| {
        let mut yt = YtBackend::new(
            crate::sources::YTMUSIC,
            &settings.ytdlp_path,
            settings.ytdlp_args.clone(),
        );
        yt.fast_client = Some(settings.ytdlp_fast_client.clone()).filter(|c| !c.is_empty());
        Arc::new(yt)
    });
    let finder = Arc::new(Sources::new(settings.duration_tolerance_seconds, backend));
    let fetcher: Arc<dyn Fetcher> = o.fetcher.unwrap_or_else(|| finder.clone());
    let library = Arc::new(Library::new(&settings.music_dir, settings.staging_dir()));
    let covers = Arc::new(Covers::new(&settings.cache_dir().join("covers")));
    let intents = Arc::new(Intents::new(
        store.clone(),
        identity.clone(),
        navidrome.clone(),
        20.0,
    ));
    let credentials = {
        let (settings, navidrome) = (settings.clone(), navidrome.clone());
        Arc::new(move || {
            if !settings.navidrome_user.is_empty() && !settings.navidrome_password.is_empty() {
                return Some(Params::new([
                    ("u", settings.navidrome_user.as_str()),
                    ("p", settings.navidrome_password.as_str()),
                    ("v", "1.16.1"),
                    ("c", "navidrome-proxy"),
                ]));
            }
            navidrome.any_credentials()
        })
    };
    let on_mapped = {
        let intents = intents.clone();
        Arc::new(move |track: Track| {
            let intents = intents.clone();
            Box::pin(async move { intents.on_track_mapped(track).await })
                as futures::future::BoxFuture<'static, ()>
        })
    };
    let indexer = Arc::new(Indexer::new(
        store.clone(),
        navidrome.clone(),
        credentials,
        settings.scan_debounce_seconds,
        Some(on_mapped),
    ));
    let tools = Tools {
        ffmpeg: settings.ffmpeg_path.clone(),
        ffprobe: settings.ffprobe_path.clone(),
        fpcalc: settings.fpcalc_path.clone(),
    };
    let pipeline = Arc::new(Pipeline {
        settings: settings.clone(),
        store: store.clone(),
        identity: identity.clone(),
        deezer: deezer.clone(),
        fetcher: fetcher.clone(),
        library: library.clone(),
        indexer: Some(indexer.clone()),
        covers: covers.clone(),
        tools: tools.clone(),
        lyrics_url: o.lyrics_url,
        background: Arc::new(Background::default()),
    });
    let scheduler = Arc::new(Scheduler::new(
        store.clone(),
        settings.background_workers,
        settings.priority_workers,
    ));
    let catalog = Arc::new(Catalog::new(
        store.clone(),
        identity.clone(),
        navidrome.clone(),
    ));
    let importer = Arc::new(Importer::new(
        store.clone(),
        identity.clone(),
        catalog.clone(),
        deezer.clone(),
        fetcher,
        scheduler.clone(),
        navidrome.clone(),
    ));
    let reconciler = Arc::new(Reconciler::new(
        store.clone(),
        identity.clone(),
        deezer.clone(),
        6.0 * 3600.0,
        50,
    ));
    let upgrader = (settings.slskd_enabled() && settings.slskd_upgrades).then(|| {
        Arc::new(Upgrader {
            store: store.clone(),
            library: library.clone(),
            client: Arc::new(Slskd::new(
                &settings.slskd_url,
                &settings.slskd_api_key,
                &settings.slskd_downloads_dir,
            )),
            min_peers: settings.slskd_min_peers,
            covers: Some(covers.clone()),
            tools,
        })
    });
    let services = Arc::new(Services {
        settings,
        store,
        identity,
        navidrome,
        deezer,
        finder,
        library,
        indexer,
        covers,
        pipeline,
        scheduler,
        catalog,
        intents,
        importer,
        reconciler,
        upgrader,
        background: Arc::new(Background::default()),
        search_pages: Mutex::new(Default::default()),
        started: crate::util::now(),
        backups: Mutex::new(None),
    });
    wire_jobs(&services);
    Ok(services)
}

fn wire_jobs(s: &Services) {
    let pipeline = s.pipeline.clone();
    s.scheduler.set_handler(
        "download",
        Arc::new(move |job, cancel| {
            let pipeline = pipeline.clone();
            Box::pin(async move { pipeline.download(&job.track_id, &cancel).await.map(Some) })
        }),
    );
    if let Some(upgrader) = &s.upgrader {
        s.scheduler.set_handler("upgrade", upgrader.handler());
    }
    // Weak: the scheduler owns this hook.
    let scheduler = Arc::downgrade(&s.scheduler);
    s.scheduler.add_done_hook(Arc::new(move |job, track| {
        let scheduler = scheduler.clone();
        Box::pin(async move {
            let Some(scheduler) = scheduler.upgrade() else {
                return;
            };
            let reason = job.reason.unwrap_or_default();
            // A like, a playlist add or an album like that pinned this track asks for
            // the best available quality: queue a background source upgrade.
            if job.kind == "download"
                && (reason.starts_with("like") || reason.starts_with("playlist"))
                && scheduler.has_handler("upgrade")
            {
                scheduler.request(&track.id, BACKGROUND, "upgrade", "upgrade", false);
            }
        })
    }));
}

pub async fn startup(s: &Arc<Services>) -> anyhow::Result<()> {
    let problems = s.store.integrity(true);
    if !problems.is_empty() {
        anyhow::bail!(
            "state database {} is corrupt: {:?}",
            s.settings.db_path().display(),
            &problems[..problems.len().min(3)]
        );
    }
    s.navidrome.refresh_identity().await;
    let removed = s.library.cleanup_staging(6.0 * 3600.0);
    if removed > 0 {
        tracing::info!("removed {removed} stale staging folders");
    }
    s.store.cache_purge_expired();
    s.indexer.start();
    s.scheduler.start();
    s.importer.start();
    s.reconciler.start();
    s.indexer.notify();
    if s.settings.backup_hours > 0.0 {
        let me = s.clone();
        *s.backups.lock() = Some(tokio::spawn(async move { backups(&me).await }));
    }
    Ok(())
}

/// A verified copy right after start, then every `backup_hours`.
async fn backups(s: &Services) {
    loop {
        let (store, dir, keep) = (
            s.store.clone(),
            s.settings.backup_dir(),
            s.settings.backup_keep,
        );
        match tokio::task::spawn_blocking(move || store.backup(&dir, keep)).await {
            Ok(Ok(Some(dest))) => tracing::info!(
                "state database backed up to {}",
                dest.file_name().and_then(|n| n.to_str()).unwrap_or("")
            ),
            Ok(Ok(None)) => tracing::error!("state database failed its check; backup skipped"),
            Ok(Err(e)) => tracing::error!("state database backup failed: {e}"),
            Err(e) => tracing::error!("state database backup failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs_f64(s.settings.backup_hours * 3600.0)).await;
    }
}

pub async fn shutdown(s: &Arc<Services>) {
    if let Some(task) = s.backups.lock().take() {
        task.abort();
    }
    s.importer.stop().await;
    s.reconciler.stop().await;
    for background in [
        &*s.background,
        &s.intents.background,
        &*s.pipeline.background,
    ] {
        background.cancel();
    }
    s.scheduler.stop().await;
    s.indexer.stop().await;
}

// ------------------------------------------------------------------ routing
fn method_name(tail: &str) -> String {
    let name = tail.rsplit('/').next().unwrap_or("");
    name.strip_suffix(".view").unwrap_or(name).to_string()
}

fn remote(req: &Request) -> Option<std::net::IpAddr> {
    req.extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
}

async fn rest(State(s): State<Arc<Services>>, req: Request) -> Response {
    let started = Instant::now();
    let method = method_name(req.uri().path().trim_start_matches("/rest/"));
    let remote = remote(&req);
    let (parts, body) = req.into_parts();
    let body = if parts.method == Method::POST {
        axum::body::to_bytes(body, MAX_BODY)
            .await
            .unwrap_or_default()
    } else {
        Default::default()
    };
    let is_form = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| {
            c.trim()
                .to_lowercase()
                .starts_with("application/x-www-form-urlencoded")
        });
    let form = if is_form {
        String::from_utf8_lossy(&body).into_owned()
    } else {
        String::new()
    };
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_default();
    let call = Call {
        method,
        params: Params::parse(parts.uri.query().unwrap_or(""), &form),
        http_method: parts.method,
        headers: parts.headers,
        remote,
    };
    let mut handled = Ok(None);
    if api::HANDLED.contains(&call.method.as_str()) {
        let auth = s.navidrome.authenticate(&call.params).await;
        handled = if auth.ok {
            s.intents.maybe_flush(call.username(), false);
            api::dispatch(&s, &call).await
        } else {
            Ok(Some(call.error(&s, auth.code, &auth.message)))
        };
    }
    let result = match handled {
        Ok(Some(response)) => Ok(response),
        Ok(None) => {
            let incoming = Incoming {
                method: &call.http_method,
                path_and_query: &path_and_query,
                headers: &call.headers,
                remote: call.remote,
            };
            s.navidrome
                .passthrough(incoming, reqwest::Body::from(body))
                .await
        }
        Err(e) => Err(e),
    };
    let response = match result {
        Ok(r) => r,
        Err(NdError::Unavailable(_)) => call.error(&s, sr::GENERIC, "Navidrome is unavailable"),
        Err(NdError::Upstream { code, message }) => call.error(&s, code, &message),
    };
    let user = if call.username().is_empty() {
        "-"
    } else {
        call.username()
    };
    tracing::info!(
        target: "navidrome_proxy::access",
        "{user} {} {} {} {:.0}ms",
        call.http_method,
        redact(&path_and_query),
        response.status().as_u16(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    response
}

async fn relay(State(s): State<Arc<Services>>, req: Request) -> Response {
    let remote = remote(&req);
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    let incoming = Incoming {
        method: &parts.method,
        path_and_query: &path_and_query,
        headers: &parts.headers,
        remote,
    };
    match s
        .navidrome
        .passthrough(
            incoming,
            reqwest::Body::wrap_stream(body.into_data_stream()),
        )
        .await
    {
        Ok(r) => r,
        Err(_) => (StatusCode::BAD_GATEWAY, "Navidrome is unavailable").into_response(),
    }
}

pub fn router(s: Arc<Services>) -> Router {
    crate::admin::routes()
        .route("/rest/{*method}", axum::routing::any(rest))
        .fallback(relay)
        .layer(tower_http::catch_panic::CatchPanicLayer::new())
        .with_state(s)
}

/// Run the server until SIGINT/SIGTERM.
pub async fn serve(settings: Settings) -> anyhow::Result<()> {
    let (host, port) = (settings.host.clone(), settings.port);
    let services = build_services(settings, Overrides::default())?;
    startup(&services).await?;
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!("listening on {host}:{port}");
    let app = router(services.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(signal())
        .await;
    shutdown(&services).await;
    Ok(result?)
}

async fn signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

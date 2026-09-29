"""The aiohttp application: wiring, routing and lifecycle."""

from __future__ import annotations

import logging
import time

from aiohttp import web

from . import api
from .catalog import Catalog
from .config import Settings
from .context import Call, Services
from .covers import Covers
from .db import Store
from .deezer import Deezer
from .identity import Identity
from .intents import Intents
from .library import Indexer, Library
from .navidrome import Navidrome, UpstreamUnavailable
from .pipeline import Pipeline
from .scheduler import BACKGROUND, Scheduler
from .sources import Sources
from .subsonic import response as sr
from .subsonic.params import Params, redact

log = logging.getLogger(__name__)
access = logging.getLogger("navidrome_proxy.access")

SERVICES = web.AppKey("services", Services)


def build_services(settings: Settings, **overrides) -> Services:
    store = overrides.get("store") or Store(settings.db_path)
    identity = Identity(store)
    navidrome = overrides.get("navidrome") or Navidrome(
        settings.navidrome_url, sr.ServerIdentity(), settings.auth_cache_seconds
    )
    deezer = overrides.get("deezer") or Deezer(
        store, settings.deezer_requests_per_second, search_ttl=settings.search_cache_seconds
    )
    finder = overrides.get("finder") or Sources(settings.duration_tolerance_seconds)
    library = Library(settings.music_dir, settings.staging_dir)
    covers = overrides.get("covers") or Covers(settings.cache_dir / "covers")
    intents = Intents(store, identity, navidrome)

    def background_credentials() -> Params | None:
        if settings.navidrome_user and settings.navidrome_password:
            return Params(
                [
                    ("u", settings.navidrome_user),
                    ("p", settings.navidrome_password),
                    ("v", "1.16.1"),
                    ("c", "navidrome-proxy"),
                ]
            )
        return next(iter(navidrome.user_credentials.values()), None)

    indexer = Indexer(
        store,
        navidrome,
        background_credentials,
        settings.scan_debounce_seconds,
        on_mapped=intents.on_track_mapped,
    )
    pipeline = Pipeline(
        settings,
        store,
        identity,
        deezer,
        finder,
        library,
        indexer,
        covers,
        **({"downloader": overrides["downloader"]} if "downloader" in overrides else {}),
    )
    scheduler = Scheduler(store, settings.background_workers, settings.priority_workers)
    catalog = Catalog(store, identity, navidrome)
    services = Services(
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
    )
    services.extra["background_credentials"] = background_credentials
    from .imports import Importer

    services.extra["importer"] = Importer(store, identity, catalog, deezer, finder, scheduler, navidrome)
    from .reconcile import Reconciler

    services.extra["reconciler"] = Reconciler(store, identity, deezer)
    if overrides.get("upgrader") is not None:
        services.extra["upgrader"] = overrides["upgrader"]
    elif settings.slskd_enabled and settings.slskd_upgrades:
        from .slskd import Slskd, Upgrader

        client = Slskd(settings.slskd_url, settings.slskd_api_key, settings.slskd_downloads_dir)
        services.extra["slskd"] = client
        services.extra["upgrader"] = Upgrader(store, library, client, settings.slskd_min_peers, covers)
    _wire_jobs(services)
    return services


def _wire_jobs(s: Services) -> None:
    async def download(job, cancel):
        return await s.pipeline.download(job.track_id, cancel)

    s.scheduler.handlers["download"] = download
    upgrader = s.extra.get("upgrader")
    if upgrader is not None:
        s.scheduler.handlers["upgrade"] = upgrader

    async def after_download(job, track):
        if job.kind != "download":
            return
        # A like, a playlist add or an album like that pinned this track asks for
        # the best available quality: queue a background source upgrade.
        if (job.reason or "").startswith(("like", "playlist")) and "upgrade" in s.scheduler.handlers:
            s.scheduler.request(track.id, BACKGROUND, "upgrade", kind="upgrade")

    s.scheduler.on_done.append(after_download)


async def _on_startup(app: web.Application) -> None:
    s = app[SERVICES]
    await s.navidrome.start()
    await s.deezer.start()
    await s.covers.start()
    await s.navidrome.refresh_identity()
    removed = s.library.cleanup_staging()
    if removed:
        log.info("removed %d stale staging folders", removed)
    s.store.cache_purge_expired()
    s.indexer.start()
    s.scheduler.start()
    s.extra["importer"].start()
    s.extra["reconciler"].start()
    s.indexer.notify()


async def _on_cleanup(app: web.Application) -> None:
    s = app[SERVICES]
    await s.extra["importer"].stop()
    await s.extra["reconciler"].stop()
    await s.scheduler.stop()
    await s.indexer.stop()
    await s.covers.close()
    if "slskd" in s.extra:
        await s.extra["slskd"].close()
    await s.deezer.close()
    await s.navidrome.close()
    s.store.close()


def _method_name(path_tail: str) -> str:
    name = path_tail.rsplit("/", 1)[-1]
    return name[:-5] if name.endswith(".view") else name


async def rest(request: web.Request) -> web.StreamResponse:
    s = request.app[SERVICES]
    method = _method_name(request.match_info["method"])
    body = await request.read() if request.method == "POST" else b""
    form = ""
    if body and request.content_type == "application/x-www-form-urlencoded":
        form = body.decode("utf-8", errors="replace")
    call = Call(request, method, Params.parse(request.query_string, form), body)
    started = time.monotonic()
    handler = api.HANDLERS.get(method)
    response: web.StreamResponse | None = None
    try:
        if handler is not None:
            auth = await s.navidrome.authenticate(call.params)
            if not auth.ok:
                response = call.error(s.navidrome.identity, auth.code, auth.message)
            else:
                s.intents.maybe_flush(call.username)
                response = await handler(s, call)
        if response is None:
            response = await s.navidrome.passthrough(request, body=body)
    except UpstreamUnavailable:
        response = call.error(s.navidrome.identity, sr.GENERIC, "Navidrome is unavailable")
    except ConnectionResetError:
        raise
    except Exception:
        log.exception("%s failed", method)
        response = call.error(s.navidrome.identity, sr.GENERIC, "Internal proxy error")
    access.info(
        "%s %s %s %d %.0fms",
        call.username or "-",
        request.method,
        redact(request.path_qs),
        response.status,
        (time.monotonic() - started) * 1000,
    )
    return response


async def relay(request: web.Request) -> web.StreamResponse:
    s = request.app[SERVICES]
    try:
        return await s.navidrome.passthrough(request)
    except UpstreamUnavailable:
        return web.Response(status=502, text="Navidrome is unavailable")


def create_app(settings: Settings, **overrides) -> web.Application:
    api.load()
    app = web.Application(client_max_size=4 * 1024 * 1024)
    app[SERVICES] = build_services(settings, **overrides)
    from . import admin

    admin.setup(app)
    app.router.add_route("*", "/rest/{method:.+}", rest)
    app.router.add_route("*", "/{tail:.*}", relay)
    app.on_startup.append(_on_startup)
    app.on_cleanup.append(_on_cleanup)
    return app

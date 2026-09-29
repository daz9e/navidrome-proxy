"""The proxy's own HTTP API under ``/proxy/v1`` (authenticated with Subsonic credentials)."""

from __future__ import annotations

import time

from aiohttp import web

from .subsonic.params import Params

STARTED = time.time()


def _services(request: web.Request):
    from .app import SERVICES

    return request.app[SERVICES]


async def authenticated(request: web.Request) -> tuple[Params | None, web.Response | None]:
    s = _services(request)
    body = ""
    if request.content_type == "application/x-www-form-urlencoded":
        body = (await request.read()).decode("utf-8", errors="replace")
    params = Params.parse(request.query_string, body if "u=" in body else "")
    result = await s.navidrome.authenticate(params)
    if result.ok:
        return params, None
    status = 503 if result.unavailable else 401
    return None, web.json_response({"error": result.message}, status=status)


async def status(request: web.Request) -> web.Response:
    params, denied = await authenticated(request)
    if denied:
        return denied
    s = _services(request)
    return web.json_response(
        {
            "uptime_seconds": round(time.time() - STARTED),
            "tracks": s.store.status_counts(),
            "jobs": s.store.job_counts(),
            "running": [f"{kind}:{track}" for track, kind in s.scheduler.running()],
            "pending": {
                "stars": len(s.store.pending_stars(params.username)),
                "scrobbles": len(s.store.pending_scrobbles(params.username)),
                "playlist_adds": len(s.store.pending_playlist_adds(params.username)),
            },
        }
    )


async def health(request: web.Request) -> web.Response:
    return web.json_response({"ok": True})


def setup(app: web.Application) -> None:
    app.router.add_get("/proxy/v1/health", health)
    app.router.add_get("/proxy/v1/status", status)

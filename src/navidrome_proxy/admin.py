"""The proxy's own HTTP API under ``/proxy/v1`` and the import page.

Authentication uses the caller's Navidrome account, given as Subsonic parameters
in the query string (``u`` + ``p``/``t``/``s``) or as ``u``/``p`` fields of a JSON body.
"""

from __future__ import annotations

import json
import time
from importlib import resources

from aiohttp import web

from .subsonic.params import Params

STARTED = time.time()


def _services(request: web.Request):
    from .app import SERVICES

    return request.app[SERVICES]


async def _payload(request: web.Request) -> tuple[dict, str]:
    """Parsed JSON body (or {}) and the raw text body for text/plain posts."""
    if not request.can_read_body:
        return {}, ""
    raw = await request.text()
    if request.content_type == "application/json":
        try:
            data = json.loads(raw or "{}")
        except ValueError as exc:
            raise web.HTTPBadRequest(
                text=json.dumps({"error": f"invalid JSON: {exc}"}), content_type="application/json"
            ) from exc
        return (data if isinstance(data, dict) else {}), ""
    if request.content_type == "application/x-www-form-urlencoded":
        form = dict(Params.parse(raw).items())
        return form, ""
    return {}, raw


async def authenticate(request: web.Request, payload: dict) -> Params | web.Response:
    s = _services(request)
    params = Params.parse(request.query_string)
    if not params.has_credentials and payload.get("u"):
        params = Params([(k, str(payload[k])) for k in ("u", "p", "t", "s") if payload.get(k)])
    result = await s.navidrome.authenticate(params)
    if result.ok:
        return params
    return web.json_response({"error": result.message}, status=503 if result.unavailable else 401)


async def status(request: web.Request) -> web.Response:
    auth = await authenticate(request, {})
    if isinstance(auth, web.Response):
        return auth
    s = _services(request)
    return web.json_response(
        {
            "uptime_seconds": round(time.time() - STARTED),
            "tracks": s.store.status_counts(),
            "jobs": s.store.job_counts(),
            "running": [f"{kind}:{track}" for track, kind in s.scheduler.running()],
            "pending": {
                "stars": len(s.store.pending_stars(auth.username)),
                "scrobbles": len(s.store.pending_scrobbles(auth.username)),
                "playlist_adds": len(s.store.pending_playlist_adds(auth.username)),
            },
        }
    )


async def create_import(request: web.Request) -> web.Response:
    payload, raw = await _payload(request)
    auth = await authenticate(request, payload)
    if isinstance(auth, web.Response):
        return auth
    text = str(payload.get("text") or raw or "")
    if len(text.encode()) > 1024 * 1024:
        return web.json_response({"error": "list is larger than 1 MiB"}, status=413)
    query = Params.parse(request.query_string)
    like = str(payload.get("like", query.get("like", "true"))).lower() not in ("false", "0", "no")
    playlist = payload.get("playlist") or query.get("playlist") or None
    importer = _services(request).extra["importer"]
    try:
        report = importer.create(auth.username, text, like=like, playlist=playlist)
    except ValueError as exc:
        return web.json_response({"error": str(exc)}, status=400)
    return web.json_response(
        {"id": report["id"], "status": report["status"], "progress": report["progress"]}, status=202
    )


async def get_import(request: web.Request) -> web.Response:
    auth = await authenticate(request, {})
    if isinstance(auth, web.Response):
        return auth
    s = _services(request)
    imp = s.store.get_import(request.match_info["id"])
    if imp is None or imp["username"] != auth.username:
        return web.json_response({"error": "not found"}, status=404)
    return web.json_response(s.extra["importer"].report(imp["id"]))


async def list_imports(request: web.Request) -> web.Response:
    auth = await authenticate(request, {})
    if isinstance(auth, web.Response):
        return auth
    s = _services(request)
    reports = [s.extra["importer"].report(i["id"]) for i in s.store.list_imports(auth.username)]
    return web.json_response(
        [{k: r[k] for k in ("id", "status", "created_at", "progress", "counts", "options")} for r in reports]
    )


async def import_page(request: web.Request) -> web.Response:
    html = resources.files("navidrome_proxy").joinpath("static/import.html").read_text(encoding="utf-8")
    return web.Response(text=html, content_type="text/html")


async def health(request: web.Request) -> web.Response:
    return web.json_response({"ok": True})


def setup(app: web.Application) -> None:
    app.router.add_get("/proxy/v1/health", health)
    app.router.add_get("/proxy/v1/status", status)
    app.router.add_post("/proxy/v1/imports", create_import)
    app.router.add_get("/proxy/v1/imports", list_imports)
    app.router.add_get("/proxy/v1/imports/{id}", get_import)
    app.router.add_get("/proxy/import", import_page)

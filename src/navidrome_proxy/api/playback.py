"""Playing external tracks, and translating ids in playback bookkeeping calls."""

from __future__ import annotations

import asyncio
import logging
import time
from pathlib import Path
from urllib.parse import quote

from aiohttp import web

from .. import ids
from ..context import Call, Services
from ..db import Track
from ..deezer import DeezerError
from ..scheduler import PLAY, PREFETCH
from ..subsonic import models
from ..subsonic import response as sr
from . import endpoint
from .browse import ensure_track

log = logging.getLogger(__name__)


def wants_transcoding(call: Call, track: Track) -> bool:
    fmt = call.params.get("format").lower()
    if fmt and fmt not in ("raw", track.audio.get("suffix", "")):
        return True
    max_rate = call.params.int("maxBitRate", 0)
    return 0 < max_rate < int(track.audio.get("bitrate") or 0)


_background: set[asyncio.Task] = set()


def prefetch_next(s: Services, track: Track) -> None:
    """Queue the next tracks of the album so album playback never stalls."""
    if s.settings.prefetch_next_tracks <= 0 or not track.album_id:
        return
    task = asyncio.create_task(_prefetch(s, track))
    _background.add(task)
    task.add_done_callback(_background.discard)


async def _prefetch(s: Services, track: Track) -> None:
    rows = s.deezer.cached_album_tracks(track.album_id)
    if rows is None:
        try:
            rows = await s.deezer.album_tracks(track.album_id)
        except DeezerError:
            return
    track_ids = s.catalog.remember_deezer(rows)
    try:
        position = track_ids.index(track.id)
    except ValueError:
        return
    for next_id in track_ids[position + 1 : position + 1 + s.settings.prefetch_next_tracks]:
        nxt = s.store.get_track(s.identity.canonical(next_id))
        if nxt is not None and not nxt.is_ready:
            s.scheduler.request(nxt.id, PREFETCH, "prefetch")


async def serve_file(s: Services, call: Call, track: Track) -> web.StreamResponse:
    path = Path(track.file or "")
    root = s.settings.music_dir.resolve()
    resolved = path.resolve()
    if root not in resolved.parents or not resolved.is_file():
        return call.error(s.navidrome.identity, sr.NOT_FOUND, "File not found")
    suffix = resolved.suffix.lstrip(".").lower()
    headers = {
        "Content-Type": models.CONTENT_TYPES.get(suffix, "application/octet-stream"),
        "Accept-Ranges": "bytes",
        "Cache-Control": "no-cache",
    }
    if call.method == "download":
        headers["Content-Disposition"] = f"attachment; filename*=UTF-8''{quote(resolved.name)}"
    return web.FileResponse(resolved, chunk_size=256 * 1024, headers=headers)


@endpoint("stream", "download")
async def stream(s: Services, call: Call):
    requested = call.params.get("id")
    vid = ids.parse(requested)
    if vid is None or not vid.is_track:
        return None
    track = await ensure_track(s, requested)
    if track is None:
        return call.error(s.navidrome.identity, sr.NOT_FOUND, "Song not found")
    if not track.is_ready:
        started = time.monotonic()
        s.scheduler.request(track.id, PLAY, "play", fresh=track.status == "failed")
        prefetch_next(s, track)
        ready = await s.scheduler.wait(track.id, s.settings.stream_wait_seconds)
        if ready is None:
            fresh = s.store.get_track(track.id)
            reason = (fresh.error if fresh else None) or "download did not finish in time"
            return call.error(s.navidrome.identity, sr.NOT_FOUND, f"Track is not available: {reason}")
        log.info("first play of %s ready after %.1fs", requested, time.monotonic() - started)
        track = ready
    else:
        prefetch_next(s, track)
    if track.local_id and wants_transcoding(call, track):
        query = call.params.replace("id", [track.local_id]).encode()
        return await s.navidrome.passthrough(call.request, path="/rest/stream", query=query, body=b"")
    return await serve_file(s, call, track)


def _translate(s: Services, values: list[str]) -> tuple[list[str], list[str]]:
    """Map ids to Navidrome ids. Returns (translated, external ids still unmapped)."""
    translated, pending = [], []
    for value in values:
        vid = ids.parse(value)
        if vid is None:
            translated.append(value)
            continue
        track = s.store.get_track(s.identity.canonical(value))
        if track is not None and track.local_id and track.is_ready:
            translated.append(track.local_id)
        else:
            pending.append(value)
    return translated, pending


@endpoint("scrobble")
async def scrobble(s: Services, call: Call):
    values = call.params.getall("id")
    if not any(ids.is_virtual(v) for v in values):
        return None
    times = call.params.getall("time")
    submission = call.params.get("submission", "true").lower() != "false"
    now_ms = int(time.time() * 1000)
    local_ids, local_times = [], []
    for index, value in enumerate(values):
        when = int(times[index]) if index < len(times) and times[index].isdigit() else now_ms
        translated, pending = _translate(s, [value])
        if translated:
            local_ids.append(translated[0])
            local_times.append(str(when))
        elif pending and submission:
            s.store.add_pending_scrobble(call.username, value, when, True)
    if local_ids:
        repeated = {"id": local_ids}
        if times:
            repeated["time"] = local_times
        await s.navidrome.call_many(
            "scrobble", call.params, repeated, submission="true" if submission else "false"
        )
    return call.ok(s.navidrome.identity)


@endpoint("savePlayQueue")
async def save_play_queue(s: Services, call: Call):
    values = call.params.getall("id")
    current = call.params.get("current")
    if not any(ids.is_virtual(v) for v in values + [current]):
        return None
    translated, _ = _translate(s, values)
    if not translated:
        return call.ok(s.navidrome.identity)
    params = call.params.replace("id", translated)
    current_ids, _ = _translate(s, [current]) if current else ([], [])
    params = params.replace("current", current_ids[:1])
    if not current_ids:
        params = params.replace("position", [])
    query = params.encode()
    return await s.navidrome.passthrough(call.request, path="/rest/savePlayQueue", query=query, body=b"")


@endpoint("savePlayQueueByIndex")
async def save_play_queue_by_index(s: Services, call: Call):
    values = call.params.getall("id")
    if not any(ids.is_virtual(v) for v in values):
        return None
    index = call.params.int("currentIndex", 0)
    kept: list[str] = []
    new_index = 0
    for position, value in enumerate(values):
        translated, _ = _translate(s, [value])
        if translated:
            if position <= index:
                new_index = len(kept)
            kept.append(translated[0])
    if not kept:
        return call.ok(s.navidrome.identity)
    params = call.params.replace("id", kept).with_(currentIndex=str(new_index))
    return await s.navidrome.passthrough(
        call.request, path="/rest/savePlayQueueByIndex", query=params.encode(), body=b""
    )

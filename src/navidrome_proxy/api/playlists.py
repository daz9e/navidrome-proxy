"""Playlists with external tracks.

Navidrome playlists can only hold library songs. Adding an external track queues
its download (like a like does) and keeps a per-user intent; the song is added
to the real playlist as soon as it is indexed. Meanwhile ``getPlaylist`` shows it
at the end of the playlist so the addition is visible right away.
"""

from __future__ import annotations

from .. import ids
from ..context import Call, Services
from ..navidrome import UpstreamError
from ..scheduler import LIKE
from . import endpoint
from .browse import ensure_track


def _split(s: Services, values: list[str]) -> tuple[list[str], list[str]]:
    local, pending = [], []
    for value in values:
        if not ids.is_virtual(value):
            local.append(value)
            continue
        track = s.store.get_track(s.identity.canonical(value))
        if track is not None and track.is_ready and track.local_id:
            local.append(track.local_id)
        else:
            pending.append(value)
    return local, pending


async def _queue(s: Services, call: Call, playlist_id: str, pending: list[str]) -> None:
    for value in pending:
        track = await ensure_track(s, value)
        if track is None:
            continue
        s.store.add_pending_playlist_add(call.username, playlist_id, value)
        s.scheduler.request(track.id, LIKE, f"playlist:{call.username}", fresh=track.status == "failed")


@endpoint("createPlaylist")
async def create_playlist(s: Services, call: Call):
    values = call.params.getall("songId")
    if not any(ids.is_virtual(v) for v in values):
        return None
    local, pending = _split(s, values)
    extra = {k: v for k, v in call.params.simple().items() if k in ("name", "playlistId")}
    try:
        root = await s.navidrome.call_many("createPlaylist", call.params, {"songId": local}, **extra)
    except UpstreamError as exc:
        return call.error(s.navidrome.identity, exc.code, exc.message)
    playlist = root.get("playlist") or {}
    playlist_id = str(playlist.get("id") or call.params.get("playlistId"))
    if playlist_id:
        await _queue(s, call, playlist_id, pending)
    return call.ok(s.navidrome.identity, {"playlist": playlist} if playlist else None)


@endpoint("updatePlaylist")
async def update_playlist(s: Services, call: Call):
    values = call.params.getall("songIdToAdd")
    if not any(ids.is_virtual(v) for v in values):
        return None
    local, pending = _split(s, values)
    params = call.params.replace("songIdToAdd", local)
    query = params.encode()
    playlist_id = call.params.get("playlistId")
    await _queue(s, call, playlist_id, pending)
    return await s.navidrome.passthrough(call.request, path="/rest/updatePlaylist", query=query, body=b"")


@endpoint("getPlaylist")
async def get_playlist(s: Services, call: Call):
    playlist_id = call.params.get("id")
    pending = [t for p, t in s.store.pending_playlist_adds(call.username) if p == playlist_id]
    if not pending or call.params.get("f", "xml").lower() not in ("json", "jsonp"):
        return None
    root = await s.navidrome.call("getPlaylist", call.params, id=playlist_id)
    playlist = root.get("playlist") or {}
    entries = list(playlist.get("entry") or [])
    present = {str(e.get("id")) for e in entries}
    extra = [
        x for x in await s.catalog.present(pending, call.params.credentials()) if str(x["id"]) not in present
    ]
    playlist["entry"] = entries + extra
    playlist["songCount"] = len(playlist["entry"])
    playlist["duration"] = sum(int(e.get("duration") or 0) for e in playlist["entry"])
    return call.ok(s.navidrome.identity, {"playlist": playlist})

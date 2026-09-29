"""Likes (star/unstar), ratings and the starred lists.

Navidrome is the owner of likes. Liking an external track downloads it with
priority and keeps a per-user intent; once the file is indexed the like is set in
Navidrome itself and the intent disappears. Until then the track shows up in that
user's starred list as a virtual song, so the like is visible immediately.
"""

from __future__ import annotations

import logging

from .. import ids
from ..context import Call, Services
from ..navidrome import UpstreamError
from ..scheduler import BACKGROUND, LIKE
from ..subsonic import models
from . import endpoint
from .browse import ensure_track

log = logging.getLogger(__name__)

PARAMS = ("id", "albumId", "artistId")


def _local(s: Services, value: str) -> str | None:
    track = s.store.get_track(s.identity.canonical(value))
    return track.local_id if track is not None and track.is_ready and track.local_id else None


@endpoint("star", "unstar")
async def star(s: Services, call: Call):
    values = {key: call.params.getall(key) for key in PARAMS}
    if not any(ids.is_virtual(v) for vs in values.values() for v in vs):
        return None
    starring = call.method == "star"
    forward: dict[str, list[str]] = {key: [] for key in PARAMS}
    for key, items in values.items():
        for value in items:
            vid = ids.parse(value)
            if vid is None:
                forward[key].append(value)
            elif vid.is_track:
                local_id = _local(s, value)
                if local_id:
                    forward["id"].append(local_id)
                    s.store.remove_pending_star(call.username, value)
                    s.catalog.forget_song(call.username, local_id)
                elif starring:
                    await _like_track(s, call, value)
                else:
                    s.store.remove_pending_star(call.username, value)
            elif vid.kind == "album":
                if starring:
                    await _like_album(s, call, vid.key)
                else:
                    s.store.remove_pending_star(call.username, value)
                    local_album = await s.intents._local_album(vid.key, call.params.credentials())
                    if local_album:
                        forward["albumId"].append(local_album)
            elif vid.kind == "artist":
                if starring:
                    s.store.add_pending_star(call.username, value)
                else:
                    s.store.remove_pending_star(call.username, value)
    forward = {k: v for k, v in forward.items() if v}
    if forward:
        try:
            await s.navidrome.call_many(call.method, call.params, forward)
        except UpstreamError as exc:
            return call.error(s.navidrome.identity, exc.code, exc.message)
    if starring:
        s.intents.maybe_flush(call.username, force=True)
    return call.ok(s.navidrome.identity)


async def _like_track(s: Services, call: Call, value: str) -> None:
    track = await ensure_track(s, value)
    s.store.add_pending_star(call.username, value)
    if track is not None:
        s.scheduler.request(track.id, LIKE, f"like:{call.username}", fresh=track.status == "failed")


async def _like_album(s: Services, call: Call, album_id: str) -> None:
    """Liking an external album downloads all its tracks in the background."""
    s.store.add_pending_star(call.username, ids.album(album_id))
    try:
        data = await s.deezer.album(album_id)
        rows = await s.deezer.album_tracks(album_id)
    except Exception as exc:
        log.warning("album %s unavailable for like: %s", album_id, exc)
        return
    for track_id in s.catalog.remember_deezer(rows, album=data):
        track = s.store.get_track(s.identity.canonical(track_id))
        if track is not None and not track.is_ready:
            s.scheduler.request(track.id, BACKGROUND, f"like-album:{call.username}")


@endpoint("setRating")
async def set_rating(s: Services, call: Call):
    value = call.params.get("id")
    if not ids.is_virtual(value):
        return None
    local_id = _local(s, value)
    if local_id is None:
        return call.ok(s.navidrome.identity)
    try:
        await s.navidrome.call("setRating", call.params, id=local_id, rating=call.params.get("rating", "0"))
    except UpstreamError as exc:
        return call.error(s.navidrome.identity, exc.code, exc.message)
    return call.ok(s.navidrome.identity)


@endpoint("getStarred", "getStarred2")
async def get_starred(s: Services, call: Call):
    pending = s.store.pending_stars(call.username)
    if not pending:
        return None
    if call.params.get("f", "xml").lower() not in ("json", "jsonp"):
        return None
    key = "starred2" if call.method == "getStarred2" else "starred"
    extra = {k: v for k, v in call.params.simple().items() if k == "musicFolderId"}
    root = await s.navidrome.call(call.method, call.params, **extra)
    starred = root.get(key) or {}
    songs = list(starred.get("song") or [])
    albums = list(starred.get("album") or [])
    artists = list(starred.get("artist") or [])
    local_song_ids = {str(x.get("id")) for x in songs}
    for _, target, created in pending:
        vid = ids.parse(target)
        if vid is None:
            continue
        if vid.is_track:
            track = s.store.get_track(s.identity.canonical(target)) or s.store.get_track(target)
            if track is None or (track.local_id and track.local_id in local_song_ids):
                continue
            songs.append(s.catalog.virtual_song(target, track, created))
        elif vid.kind == "album":
            data = s.deezer.cached_album(vid.key)
            if data:
                albums.append(models.album(target, data, starred=created))
        elif vid.kind == "artist":
            try:
                data = await s.deezer.artist(vid.key)
            except Exception:
                continue
            artists.append(models.artist(target, data, starred=created))
    result = {k: v for k, v in (("artist", artists), ("album", albums), ("song", songs)) if v}
    return call.ok(s.navidrome.identity, {key: result})

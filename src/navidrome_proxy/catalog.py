"""Presenting external tracks to clients.

The single rule: once a track is in the library *and* indexed by Navidrome, it is
listed as the real Navidrome song (its own id, likes, play counts, cover art).
Only tracks that are not local yet appear with a virtual ``deezer:``/``yt:``/``sc:``
id. A client therefore never sees the same song twice under two ids.
"""

from __future__ import annotations

import asyncio
import logging
import time
from typing import Any

from . import deezer as dz
from . import ids
from .db import Store, Track
from .identity import Identity
from .navidrome import Navidrome, UpstreamError, UpstreamUnavailable
from .subsonic import models
from .subsonic.params import Params

log = logging.getLogger(__name__)


class Catalog:
    def __init__(self, store: Store, identity: Identity, navidrome: Navidrome):
        self.store = store
        self.identity = identity
        self.navidrome = navidrome
        self._song_cache: dict[tuple[str, str], tuple[float, dict]] = {}

    # ------------------------------------------------------------ remember
    def remember_deezer(self, rows: list[dict], album: dict | None = None) -> list[str]:
        """Store Deezer rows; a search row never overwrites a complete (hydrated) record."""
        track_ids = [ids.track(row["id"]) for row in rows if row.get("id")]
        known = self.store.get_tracks(track_ids)
        items = []
        for row in rows:
            if not row.get("id"):
                continue
            track_id = ids.track(row["id"])
            meta = dz.track_meta(row, album)
            if not meta["full"]:
                existing = known.get(track_id)
                if existing is not None and existing.meta.get("full"):
                    continue
                meta.pop("full")
            items.append((track_id, meta))
        self.store.remember_tracks(items)
        return track_ids

    def remember_ytmusic(self, rows: list[dict]) -> list[str]:
        items = []
        for row in rows:
            meta = {
                "source": "youtube",
                "title": row["title"],
                "artists": row["artists"],
                "artist": ", ".join(row["artists"]) or "YouTube",
                "album": row.get("album"),
                "duration": row.get("duration"),
                "cover": row.get("thumbnail"),
                "explicit": row.get("explicit"),
            }
            items.append((ids.youtube(row["video_id"]), meta))
        self.store.remember_tracks(items)
        return [track_id for track_id, _ in items]

    # ------------------------------------------------------------- present
    def starred_map(self, username: str) -> dict[str, float]:
        return {target: created for _, target, created in self.store.pending_stars(username)}

    def virtual_song(self, requested_id: str, track: Track, starred: float | None = None) -> dict[str, Any]:
        audio = track.audio if track.is_ready else None
        return models.song(requested_id, track.meta, audio=audio, created=track.created_at, starred=starred)

    async def navidrome_songs(self, local_ids: list[str], creds: Params) -> dict[str, dict]:
        """Navidrome song objects (with the caller's stars) for local ids, briefly cached."""
        now = time.monotonic()
        user = creds.username
        out: dict[str, dict] = {}
        missing = []
        for local_id in dict.fromkeys(local_ids):
            cached = self._song_cache.get((user, local_id))
            if cached and cached[0] > now:
                out[local_id] = cached[1]
            else:
                missing.append(local_id)

        async def one(local_id: str) -> None:
            try:
                root = await self.navidrome.call("getSong", creds, id=local_id, timeout=10)
            except UpstreamError as exc:
                if exc.code == 70:
                    # The song vanished from Navidrome: forget the stale mapping.
                    for track in self.store.tracks_by_local_ids([local_id]).values():
                        self.store.update_track(track.id, local_id=None)
                return
            except UpstreamUnavailable:
                return
            song = root.get("song")
            if isinstance(song, dict):
                out[local_id] = song
                self._song_cache[(user, local_id)] = (now + 60, song)

        sem = asyncio.Semaphore(8)

        async def guarded(local_id: str) -> None:
            async with sem:
                await one(local_id)

        await asyncio.gather(*(guarded(i) for i in missing))
        if len(self._song_cache) > 20_000:
            self._song_cache.clear()
        return out

    def forget_song(self, username: str, local_id: str) -> None:
        self._song_cache.pop((username, local_id), None)

    async def present(
        self, track_ids: list[str], creds: Params, exclude_local: set[str] | None = None
    ) -> list[dict[str, Any]]:
        """Songs for external ids, local objects where possible, deduplicated, order kept."""
        exclude_local = exclude_local or set()
        canonical = self.identity.canonical_many(track_ids)
        tracks = self.store.get_tracks(set(canonical.values()) | set(track_ids))
        local_ids = [t.local_id for t in tracks.values() if t.local_id and t.is_ready]
        local_songs = await self.navidrome_songs(local_ids, creds) if local_ids else {}
        starred = self.starred_map(creds.username)
        out: list[dict[str, Any]] = []
        seen: set[str] = set()
        for track_id in track_ids:
            track = tracks.get(canonical[track_id]) or tracks.get(track_id)
            if track is None:
                continue
            if track.local_id and track.local_id in local_songs:
                if track.local_id in exclude_local or track.local_id in seen:
                    continue
                seen.add(track.local_id)
                out.append(local_songs[track.local_id])
                continue
            # Not indexed yet: keep the id the caller listed, deduplicate by recording.
            if track.id in seen:
                continue
            seen.add(track.id)
            star = starred.get(track_id) or starred.get(track.id)
            out.append(self.virtual_song(track_id, track, star))
        return out

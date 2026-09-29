"""Deezer public API: the catalog behind every external track, album and artist."""

from __future__ import annotations

import asyncio
import json
import logging
import time
from collections import OrderedDict
from typing import Any

import aiohttp

from .db import Store

log = logging.getLogger(__name__)

API = "https://api.deezer.com"
COVER_URL = "https://cdn-images.dzcdn.net/images/{kind}/{md5}/{size}x{size}-000000-80-0-0.jpg"
DAY = 24 * 3600


class DeezerError(Exception):
    pass


class RateLimiter:
    """A token bucket: Deezer allows 50 requests per 5 seconds per IP."""

    def __init__(self, per_second: float, burst: int = 10):
        self.per_second = max(0.1, per_second)
        self.capacity = float(burst)
        self.tokens = float(burst)
        self.updated = time.monotonic()
        self._lock = asyncio.Lock()

    async def acquire(self) -> None:
        async with self._lock:
            while True:
                now = time.monotonic()
                self.tokens = min(self.capacity, self.tokens + (now - self.updated) * self.per_second)
                self.updated = now
                if self.tokens >= 1:
                    self.tokens -= 1
                    return
                await asyncio.sleep((1 - self.tokens) / self.per_second)


class LRU:
    def __init__(self, size: int):
        self.size = size
        self.data: OrderedDict[str, tuple[float, Any]] = OrderedDict()

    def get(self, key: str) -> Any | None:
        item = self.data.get(key)
        if item is None:
            return None
        if item[0] < time.monotonic():
            self.data.pop(key, None)
            return None
        self.data.move_to_end(key)
        return item[1]

    def put(self, key: str, value: Any, ttl: float) -> None:
        self.data[key] = (time.monotonic() + ttl, value)
        self.data.move_to_end(key)
        while len(self.data) > self.size:
            self.data.popitem(last=False)


def cover_url(md5: str | None, size: int = 1000, kind: str = "cover") -> str | None:
    if not md5:
        return None
    return COVER_URL.format(kind=kind, md5=md5, size=size)


def best_cover(item: dict | None) -> str | None:
    item = item or {}
    return (
        cover_url(item.get("md5_image"))
        or item.get("cover_xl")
        or item.get("cover_big")
        or item.get("cover_medium")
        or item.get("cover")
    )


def artist_names(track: dict) -> list[str]:
    """All performers in credit order: main artist first, then contributors."""
    names: list[str] = []
    primary = ((track.get("artist") or {}).get("name") or "").strip()
    if primary:
        names.append(primary)
    for contributor in track.get("contributors") or []:
        if not isinstance(contributor, dict):
            continue
        name = str(contributor.get("name") or "").strip()
        if name and name.casefold() not in {n.casefold() for n in names}:
            names.append(name)
    return names


def year_of(date: str | None) -> int:
    try:
        return int(str(date or "")[:4])
    except ValueError:
        return 0


def track_meta(track: dict, album: dict | None = None) -> dict[str, Any]:
    """Normalize a Deezer track (search row or full object) into proxy metadata."""
    album_obj = {**(track.get("album") or {}), **(album or {})}
    artists = artist_names(track)
    genres = [g.get("name") for g in ((album or {}).get("genres") or {}).get("data", []) if g.get("name")]
    album_artist = ((album or {}).get("artist") or {}).get("name") or (artists[0] if artists else "")
    release = album_obj.get("release_date") or track.get("release_date") or ""
    meta = {
        "source": "deezer",
        "deezer_id": str(track.get("id")),
        "title": track.get("title") or "",
        "title_version": track.get("title_version") or "",
        "artist": ", ".join(artists),
        "artists": artists,
        "artist_id": str((track.get("artist") or {}).get("id") or ""),
        "album": album_obj.get("title") or "",
        "album_id": str(album_obj.get("id") or "") or None,
        "album_artist": album_artist,
        "duration": int(track.get("duration") or 0),
        "track": int(track.get("track_position") or 0) or None,
        "disc": int(track.get("disk_number") or 0) or None,
        "release_date": release or None,
        "year": year_of(release) or None,
        "isrc": track.get("isrc") or None,
        "explicit": bool(track.get("explicit_lyrics")),
        "cover": best_cover(album_obj) or best_cover(track),
        "cover_md5": album_obj.get("md5_image") or track.get("md5_image"),
        "genre": genres[0] if genres else None,
        "track_total": int((album or {}).get("nb_tracks") or 0) or None,
        "rank": int(track.get("rank") or 0),
    }
    # Search rows carry only the main artist; a full object carries contributors.
    meta["full"] = "contributors" in track
    return meta


class Deezer:
    def __init__(self, store: Store, per_second: float = 8.0, base_url: str = API, search_ttl: float = 120.0):
        self.store = store
        self.base_url = base_url.rstrip("/")
        self.limiter = RateLimiter(per_second)
        self.search_ttl = search_ttl
        self.memory = LRU(4096)
        self._session: aiohttp.ClientSession | None = None
        self._inflight: dict[str, asyncio.Future] = {}

    async def start(self) -> None:
        self._session = aiohttp.ClientSession(
            timeout=aiohttp.ClientTimeout(total=20, connect=5),
            headers={"User-Agent": "navidrome-proxy/2"},
        )

    async def close(self) -> None:
        if self._session:
            await self._session.close()

    async def get(self, path: str, ttl: float, persist: bool = False) -> dict:
        """GET ``path`` with memory (and optionally SQLite) caching and single-flight."""
        key = f"deezer:{path}"
        cached = self.memory.get(key)
        if cached is not None:
            return cached
        if persist:
            stored = self.store.cache_get(key)
            if stored is not None:
                self.memory.put(key, stored, min(ttl, 3600))
                return stored
        if key in self._inflight:
            return await asyncio.shield(self._inflight[key])
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        self._inflight[key] = future
        try:
            data = await self._fetch(path)
            self.memory.put(key, data, ttl)
            if persist:
                self.store.cache_set(key, data, ttl)
            future.set_result(data)
            return data
        except BaseException as exc:
            future.set_exception(exc)
            future.exception()  # mark retrieved when nobody else waits
            raise
        finally:
            self._inflight.pop(key, None)

    async def _fetch(self, path: str) -> dict:
        assert self._session is not None, "Deezer.start() was not awaited"
        url = f"{self.base_url}/{path.lstrip('/')}"
        last: Exception | None = None
        for attempt in range(4):
            await self.limiter.acquire()
            try:
                async with self._session.get(url) as resp:
                    if resp.status >= 500 or resp.status == 429:
                        raise DeezerError(f"HTTP {resp.status}")
                    data = json.loads(await resp.read())
            except (aiohttp.ClientError, TimeoutError, ValueError, DeezerError) as exc:
                last = exc
                await asyncio.sleep(0.5 * (attempt + 1))
                continue
            error = data.get("error") if isinstance(data, dict) else None
            if error:
                if error.get("code") == 4:  # Quota limit exceeded
                    last = DeezerError("quota exceeded")
                    await asyncio.sleep(1.5 * (attempt + 1))
                    continue
                if error.get("code") == 800:  # no data
                    raise DeezerError(f"not found: {path}")
                raise DeezerError(f"{error.get('type')}: {error.get('message')}")
            return data
        raise DeezerError(f"{path}: {last}")

    # --------------------------------------------------------------- search
    async def search_tracks(self, query: str, limit: int = 25, index: int = 0) -> list[dict]:
        data = await self.get(f"search/track?q={_q(query)}&limit={limit}&index={index}", self.search_ttl)
        return [
            x for x in data.get("data", []) if x.get("type", "track") == "track" and x.get("readable", True)
        ]

    async def search_albums(self, query: str, limit: int = 10) -> list[dict]:
        data = await self.get(f"search/album?q={_q(query)}&limit={limit}", self.search_ttl)
        return data.get("data", [])

    async def search_artists(self, query: str, limit: int = 5) -> list[dict]:
        data = await self.get(f"search/artist?q={_q(query)}&limit={limit}", self.search_ttl)
        return data.get("data", [])

    # ---------------------------------------------------------------- lookup
    async def track(self, track_id: str | int) -> dict:
        return await self.get(f"track/{track_id}", 7 * DAY, persist=True)

    async def album(self, album_id: str | int) -> dict:
        return await self.get(f"album/{album_id}", 7 * DAY, persist=True)

    async def album_tracks(self, album_id: str | int) -> list[dict]:
        data = await self.get(f"album/{album_id}/tracks?limit=500", 7 * DAY, persist=True)
        return data.get("data", [])

    def cached_album_tracks(self, album_id: str | int) -> list[dict] | None:
        """Album tracks only if already cached; never hits the network."""
        key = f"deezer:album/{album_id}/tracks?limit=500"
        data = self.memory.get(key) or self.store.cache_get(key)
        return None if data is None else data.get("data", [])

    def cached_album(self, album_id: str | int) -> dict | None:
        key = f"deezer:album/{album_id}"
        return self.memory.get(key) or self.store.cache_get(key)

    async def artist(self, artist_id: str | int) -> dict:
        return await self.get(f"artist/{artist_id}", DAY, persist=True)

    async def artist_albums(self, artist_id: str | int) -> list[dict]:
        out: list[dict] = []
        for index in range(0, 500, 100):
            data = await self.get(f"artist/{artist_id}/albums?limit=100&index={index}", DAY, persist=True)
            page = data.get("data", [])
            out.extend(page)
            if len(page) < 100 or not data.get("next"):
                break
        return out

    async def artist_top(self, artist_id: str | int, limit: int = 10) -> list[dict]:
        data = await self.get(f"artist/{artist_id}/top?limit={limit}", DAY, persist=True)
        return data.get("data", [])

    async def artist_related(self, artist_id: str | int, limit: int = 10) -> list[dict]:
        data = await self.get(f"artist/{artist_id}/related?limit={limit}", DAY, persist=True)
        return data.get("data", [])

    async def full_track_meta(self, track_id: str | int) -> dict[str, Any]:
        """Complete metadata: full track (contributors, ISRC) plus its album (genre, totals)."""
        track = await self.track(track_id)
        album_id = (track.get("album") or {}).get("id")
        album = None
        if album_id:
            try:
                album = await self.album(album_id)
            except DeezerError as exc:
                log.info("album %s unavailable for track %s: %s", album_id, track_id, exc)
        return track_meta(track, album)


def _q(query: str) -> str:
    from urllib.parse import quote

    return quote(query, safe="")

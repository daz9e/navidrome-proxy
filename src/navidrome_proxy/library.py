"""The music directory and its reflection in Navidrome.

Layout (readable without the proxy, usable by Plex/Jellyfin and folder browsing)::

    <album artist>/<album>/<disc>-<track> - <title> [deezer-<id>].m4a
    <artist>/YouTube/<title> [yt-<video id>].m4a

The ``[deezer-<id>]`` token keeps the file tied to its catalog entry: Navidrome
reports every song's ``path``, so the proxy can find the Navidrome song id of a
download without guessing, and the mapping can be rebuilt from the files alone.
Files of the first proxy generation (flat ``deezer-<id>.m4a``) are recognized too.

Identity rule: an external id (``deezer:<id>``) is only the *primary* id while a
track is not in the library. Once Navidrome has indexed it, the Navidrome song id
is what the proxy lists everywhere; the external id stays a permanent alias.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
import shutil
import time
import unicodedata
import uuid
from collections.abc import Awaitable, Callable
from pathlib import Path

from . import ids, matching
from .db import Store, Track
from .navidrome import Navidrome, UpstreamError, UpstreamUnavailable
from .subsonic.params import Params

log = logging.getLogger(__name__)


_TOKEN = re.compile(r"\[(deezer-\d+|yt-[A-Za-z0-9_-]{11}|sc-\d+)\]")
_LEGACY = re.compile(r"^(deezer-\d+|yt-[A-Za-z0-9_-]{11})$")
_UNSAFE = re.compile(r'[\x00-\x1f/\\:*?"<>|]+')


def file_stem(track_id: str) -> str:
    """The id token embedded in file names: ``deezer-<id>`` or ``yt-<video id>``."""
    vid = ids.parse(track_id)
    if vid is None or not vid.is_track:
        raise ValueError(f"not an external track id: {track_id}")
    return {"track": "deezer", "youtube": "yt", "soundcloud": "sc"}[vid.kind] + f"-{vid.key}"


def track_id_from_path(path: str | Path) -> str | None:
    """Recover the external track id from a library file name (new or legacy)."""
    stem = Path(str(path)).stem
    match = _TOKEN.search(stem) or _LEGACY.match(stem)
    if not match:
        return None
    prefix, _, key = match.group(1).partition("-")
    return f"{prefix}:{key}"


def safe_name(value: str | None, limit: int = 80, fallback: str = "Unknown") -> str:
    text = unicodedata.normalize("NFC", str(value or "")).strip()
    text = _UNSAFE.sub(" ", text)
    text = " ".join(text.split()).strip(". ")
    if len(text.encode()) > limit * 2 or len(text) > limit:
        text = text[:limit].rstrip(". ")
    return text or fallback


def relative_path(track_id: str, meta: dict, suffix: str) -> Path:
    token = file_stem(track_id)
    suffix = suffix if suffix.startswith(".") else f".{suffix}"
    title = safe_name(meta.get("title"), 100, "Untitled")
    artists = meta.get("artists") or []
    lead = artists[0] if artists else meta.get("artist")
    if not token.startswith("deezer-") and not meta.get("album"):
        folder = "YouTube" if token.startswith("yt-") else "SoundCloud"
        return Path(safe_name(lead)) / folder / f"{title} [{token}]{suffix}"
    folder = Path(safe_name(meta.get("album_artist") or lead)) / safe_name(meta.get("album"), 80, "Singles")
    track = int(meta.get("track") or 0)
    prefix = f"{int(meta.get('disc') or 1)}-{track:02d} - " if track else ""
    return folder / f"{prefix}{title} [{token}]{suffix}"


class Library:
    def __init__(self, music_dir: Path, staging_dir: Path):
        self.music_dir = music_dir
        self.staging_dir = staging_dir

    def new_staging(self, track_id: str) -> Path:
        path = self.staging_dir / f"{file_stem(track_id)}-{uuid.uuid4().hex[:8]}"
        path.mkdir(parents=True, exist_ok=True)
        return path

    def cleanup_staging(self, max_age: float = 6 * 3600) -> int:
        """Remove leftovers of crashed downloads."""
        removed = 0
        if not self.staging_dir.is_dir():
            return 0
        cutoff = time.time() - max_age
        for child in self.staging_dir.iterdir():
            try:
                if child.stat().st_mtime < cutoff:
                    shutil.rmtree(child) if child.is_dir() else child.unlink()
                    removed += 1
            except OSError:
                continue
        return removed

    def publish(self, track_id: str, prepared: Path, meta: dict, previous: str | None = None) -> Path:
        """Atomically move a finished file into the library.

        Replacing an existing file (source upgrade) is a single rename, so a
        concurrent stream keeps reading the old inode and never sees a partial file.
        """
        dest = self.music_dir / relative_path(track_id, meta, prepared.suffix.lower())
        dest.parent.mkdir(parents=True, exist_ok=True)
        tmp = dest.with_name(f".{dest.name}.{uuid.uuid4().hex[:6]}.part")
        if prepared.stat().st_dev == self.music_dir.stat().st_dev:
            os.replace(prepared, tmp)
        else:
            shutil.copy2(prepared, tmp)
        os.chmod(tmp, 0o644)
        os.replace(tmp, dest)
        if previous and Path(previous) != dest:
            old = Path(previous)
            try:
                old.unlink()
                for sidecar in (old.with_suffix(".lrc"), old.with_suffix(".txt")):
                    if sidecar.exists() and not dest.with_suffix(sidecar.suffix).exists():
                        sidecar.rename(dest.with_suffix(sidecar.suffix))
            except FileNotFoundError:
                pass
            self._prune(old.parent)
        return dest

    def _prune(self, folder: Path) -> None:
        """Remove folders left empty by a move, never the music root itself."""
        root = self.music_dir.resolve()
        current = folder.resolve()
        while current != root and root in current.parents:
            try:
                current.rmdir()
            except OSError:
                return
            current = current.parent


CredentialsProvider = Callable[[], Params | None]


class Indexer:
    """Gets new files into Navidrome and learns their song ids.

    Scans are debounced (one ``startScan`` for a burst of imports) and song ids are
    resolved with a targeted ``search3`` filtered by the unique file name.
    """

    def __init__(
        self,
        store: Store,
        navidrome: Navidrome,
        credentials: CredentialsProvider,
        debounce: float = 3.0,
        on_mapped: Callable[[Track], Awaitable[None]] | None = None,
    ):
        self.store = store
        self.navidrome = navidrome
        self.credentials = credentials
        self.debounce = debounce
        self.on_mapped = on_mapped
        self._wake = asyncio.Event()
        self._task: asyncio.Task | None = None
        self._pending_since: float | None = None

    def start(self) -> None:
        self._task = asyncio.create_task(self._loop(), name="indexer")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    def notify(self) -> None:
        if self._pending_since is None:
            self._pending_since = time.monotonic()
        self._wake.set()

    async def _loop(self) -> None:
        backoff = 5.0
        while True:
            try:
                await asyncio.wait_for(self._wake.wait(), timeout=backoff)
            except TimeoutError:
                pass
            self._wake.clear()
            try:
                pending = self.store.ready_without_local_id(limit=200)
                if not pending:
                    self._pending_since = None
                    backoff = 60.0
                    continue
                if self._pending_since is not None:
                    wait = self.debounce - (time.monotonic() - self._pending_since)
                    if wait > 0:
                        await asyncio.sleep(wait)
                    self._pending_since = None
                    await self.scan()
                    await asyncio.sleep(1.0)
                resolved = await self.resolve(pending)
                backoff = 2.0 if resolved < len(pending) else 5.0
                # Give up polling quickly for files Navidrome refuses; a later
                # notify() or restart will retry them.
                stale = [t for t in pending if t.local_id is None and time.time() - t.updated_at > 1800]
                if len(stale) == len(pending):
                    backoff = 300.0
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("indexer iteration failed")
                backoff = 30.0

    async def scan(self) -> None:
        creds = self.credentials()
        if creds is None:
            return
        try:
            await self.navidrome.call("startScan", creds, timeout=15)
        except UpstreamError as exc:
            # Non-admin users cannot start scans; Navidrome's watcher still picks files up.
            log.debug("startScan refused: %s", exc)
        except UpstreamUnavailable as exc:
            log.warning("startScan failed: %s", exc)

    async def resolve(self, tracks: list[Track]) -> int:
        creds = self.credentials()
        if creds is None:
            return 0
        resolved = 0
        for track in tracks:
            local_id = await self.find_local_id(track, creds)
            if local_id:
                self.store.update_track(track.id, local_id=local_id)
                resolved += 1
                fresh = self.store.get_track(track.id)
                if fresh and self.on_mapped:
                    try:
                        await self.on_mapped(fresh)
                    except Exception:
                        log.exception("post-mapping hook failed for %s", track.id)
        return resolved

    async def find_local_id(self, track: Track, creds: Params) -> str | None:
        """Find the Navidrome song of a downloaded file.

        Navidrome does not report real file paths by default (``path`` is rebuilt
        from tags), so the file name cannot be used. The exact byte size of the file
        we wrote is: title + size identifies the song unambiguously.
        """
        if not track.file:
            return None
        try:
            size = Path(track.file).stat().st_size
        except OSError:
            return None
        token = file_stem(track.id)
        queries = [track.title, f"{track.title} {track.artists[0] if track.artists else ''}".strip()]
        for query in dict.fromkeys(q for q in queries if q):
            try:
                root = await self.navidrome.call(
                    "search3",
                    creds,
                    query=query,
                    songCount="100",
                    albumCount="0",
                    artistCount="0",
                    timeout=20,
                )
            except (UpstreamError, UpstreamUnavailable) as exc:
                log.debug("local id lookup failed for %s: %s", track.id, exc)
                return None
            songs = (root.get("searchResult3") or {}).get("song") or []
            by_size = [s for s in songs if int(s.get("size") or -1) == size]
            if len(by_size) == 1:
                return str(by_size[0]["id"])
            # Real paths are reported when the player is configured to do so.
            for song in songs:
                base = str(song.get("path", "")).rsplit("/", 1)[-1]
                if f"[{token}]" in base or base == Path(track.file).name:
                    return str(song["id"])
            duration = float(track.audio.get("duration") or track.duration)
            exact = [
                s
                for s in (by_size or songs)
                if matching.normalize(s.get("title")) == matching.normalize(track.title)
                and matching.normalize(s.get("album")) == matching.normalize(track.meta.get("album"))
                and abs(float(s.get("duration") or 0) - duration) <= 2
            ]
            if len(exact) == 1:
                return str(exact[0]["id"])
        return None

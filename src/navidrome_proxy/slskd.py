"""Quality upgrades from Soulseek (slskd) for liked tracks.

A liked track is first downloaded from YouTube Music (fast, AAC ~128 kbit/s). A
background job then looks for a better lossy copy on Soulseek and replaces the
file only after it proves to be the *same recording*: its Chromaprint
fingerprint must match the file we already have. Peers' file names and tags are
not trusted — that is how a "minus" (instrumental) once replaced FORTUNA 812 —
Black Water in the first proxy generation.
"""

from __future__ import annotations

import asyncio
import glob
import logging
import shutil
import time
from dataclasses import dataclass
from pathlib import Path, PureWindowsPath
from typing import Any
from urllib.parse import quote

import aiohttp

from . import audio, matching, tagging
from .db import Store, Track
from .library import Library
from .sources import SourceError

log = logging.getLogger(__name__)

LOSSY = {".mp3": 256, ".m4a": 256, ".aac": 256}
SIMILARITY_THRESHOLD = 0.88


@dataclass
class RemoteFile:
    username: str
    filename: str
    size: int
    bitrate: int
    length: float | None
    free_slot: bool
    queue: int
    peers: int = 1
    score: float = 0.0

    @property
    def basename(self) -> str:
        return PureWindowsPath(self.filename).name


def select(
    responses: list[dict[str, Any]], wanted: matching.Wanted, min_peers: int = 1, tolerance: float = 3.0
) -> list[RemoteFile]:
    """Acceptable files, best first; identical copies (name+size) are counted as peers."""
    found: list[RemoteFile] = []
    for response in responses:
        username = str(response.get("username") or "")
        for f in response.get("files") or []:
            name = str(f.get("filename") or "")
            suffix = Path(PureWindowsPath(name).name).suffix.lower()
            if suffix not in LOSSY or f.get("isLocked"):
                continue
            bitrate = int(f.get("bitRate") or 0)
            if bitrate and bitrate < LOSSY[suffix]:
                continue
            length = f.get("length")
            path = PureWindowsPath(name)
            folder = " ".join(path.parts[-3:-1])
            scored = matching.score_source_candidate(
                wanted, path.stem, folder, float(length) if length else None, tolerance=tolerance
            )
            if scored.rejected or scored.score < 55 or (wanted.duration and not length):
                continue
            found.append(
                RemoteFile(
                    username,
                    name,
                    int(f.get("size") or 0),
                    bitrate,
                    length,
                    bool(response.get("hasFreeUploadSlot")),
                    int(response.get("queueLength") or 0),
                    score=scored.score,
                )
            )
    copies: dict[tuple[str, int], int] = {}
    for rf in found:
        key = (rf.basename.casefold(), rf.size)
        copies[key] = copies.get(key, 0) + 1
    for rf in found:
        rf.peers = copies[(rf.basename.casefold(), rf.size)]
    eligible = [rf for rf in found if rf.peers >= min_peers]
    eligible.sort(key=lambda r: (-r.score, -min(r.bitrate, 320), -r.peers, not r.free_slot, r.queue))
    return eligible


class Slskd:
    def __init__(self, url: str, api_key: str, downloads_dir: Path):
        self.url = url.rstrip("/")
        self.api_key = api_key
        self.downloads_dir = downloads_dir
        self._session: aiohttp.ClientSession | None = None

    async def _http(self) -> aiohttp.ClientSession:
        if self._session is None or self._session.closed:
            self._session = aiohttp.ClientSession(
                headers={"X-API-Key": self.api_key}, timeout=aiohttp.ClientTimeout(total=30)
            )
        return self._session

    async def close(self) -> None:
        if self._session:
            await self._session.close()

    async def _json(self, method: str, path: str, **kw) -> Any:
        session = await self._http()
        try:
            async with session.request(method, f"{self.url}/api/v0/{path}", **kw) as resp:
                if resp.status >= 400:
                    raise SourceError(f"slskd {method} {path}: HTTP {resp.status}")
                if resp.status == 204:
                    return None
                return await resp.json(content_type=None)
        except (aiohttp.ClientError, TimeoutError) as exc:
            raise SourceError(f"slskd unavailable: {exc}") from exc

    async def search(self, text: str, timeout: float = 45.0, poll: float = 2.0) -> list[dict]:
        created = await self._json(
            "POST",
            "searches",
            json={
                "searchText": text,
                "searchTimeout": int(timeout * 1000),
                "responseLimit": 200,
                "filterResponses": True,
            },
        )
        search_id = created["id"]
        deadline = time.monotonic() + timeout + 10
        try:
            while time.monotonic() < deadline:
                state = await self._json("GET", f"searches/{search_id}")
                if state.get("isComplete"):
                    break
                await asyncio.sleep(poll)
            return await self._json("GET", f"searches/{search_id}/responses") or []
        finally:
            try:
                await self._json("DELETE", f"searches/{search_id}")
            except SourceError:
                pass

    async def download(self, rf: RemoteFile, timeout: float = 600.0, poll: float = 2.0) -> Path:
        # Peer names are chosen by other Soulseek users and may contain "/", "?" or "#".
        user_path = f"transfers/downloads/{quote(rf.username, safe='')}"
        await self._json("POST", user_path, json=[{"filename": rf.filename, "size": rf.size}])
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            await asyncio.sleep(poll)
            user = await self._json("GET", user_path) or {}
            for directory in user.get("directories") or []:
                for f in directory.get("files") or []:
                    if f.get("filename") != rf.filename:
                        continue
                    state = str(f.get("state") or "")
                    if "Succeeded" in state:
                        path = self.locate(rf)
                        if path is not None:
                            return path
                    elif "Completed" in state:
                        raise SourceError(f"slskd transfer {state}", transient=False)
        raise SourceError("slskd transfer timed out")

    def locate(self, rf: RemoteFile) -> Path | None:
        """slskd stores ``<downloads>/<remote folder>/<file name>``; match by name and size."""
        folder = PureWindowsPath(rf.filename).parent.name
        direct = self.downloads_dir / folder / rf.basename
        if direct.is_file() and direct.stat().st_size == rf.size:
            return direct
        # "[2019] Title.flac" is a literal name, not a glob character class.
        for path in self.downloads_dir.rglob(glob.escape(rf.basename)):
            if path.is_file() and path.stat().st_size == rf.size:
                return path
        return None


class Upgrader:
    def __init__(self, store: Store, library: Library, client: Slskd, min_peers: int = 2, covers=None):
        self.covers = covers
        self.store = store
        self.library = library
        self.client = client
        self.min_peers = min_peers

    async def __call__(self, job, cancel) -> Track | None:
        track = self.store.get_track(job.track_id)
        if track is None or not track.is_ready:
            return None
        if int(track.audio.get("bitrate") or 0) >= 256 or track.source == "soulseek":
            return track
        wanted = matching.Wanted(
            track.title, track.artists, duration=track.audio.get("duration") or track.duration
        )
        responses = await self.client.search(f"{track.artists[0] if track.artists else ''} {track.title}")
        candidates = select(responses, wanted, self.min_peers)
        if not candidates:
            log.info("no Soulseek upgrade for %s (%d responses)", track.id, len(responses))
            return track
        reference = await audio.fingerprint(Path(track.file or ""))
        if reference is None:
            raise SourceError("fpcalc unavailable: refusing an unverifiable upgrade", transient=False)
        for rf in candidates[:3]:
            try:
                upgraded = await self._try(track, rf, reference)
            except SourceError as exc:
                log.info("Soulseek candidate %s from %s rejected: %s", rf.basename, rf.username, exc)
                continue
            if upgraded is not None:
                return upgraded
        return track

    async def _try(self, track: Track, rf: RemoteFile, reference: list[int]) -> Track | None:
        raw = await self.client.download(rf)
        staging = self.library.new_staging(track.id)
        try:
            copy = staging / f"source{raw.suffix.lower()}"
            shutil.copy2(raw, copy)
            prepared, info = await audio.normalize(copy, staging, "prepared")
            current = float(track.audio.get("duration") or track.duration or 0)
            if current and abs(info.duration - current) > 3:
                raise SourceError(f"duration {info.duration:.0f}s vs {current:.0f}s", transient=False)
            candidate = await audio.fingerprint(prepared)
            similarity = audio.fingerprint_similarity(reference, candidate or [])
            if similarity < SIMILARITY_THRESHOLD:
                raise SourceError(f"different recording (similarity {similarity:.2f})", transient=False)
            found = await self.covers.get(track.meta.get("cover")) if self.covers else None
            cover = found[0] if found else None
            await asyncio.to_thread(tagging.write_tags, prepared, track.meta, cover)
            dest = self.library.publish(track.id, prepared, track.meta, previous=track.file)
            self.store.update_track(
                track.id,
                file=str(dest),
                source="soulseek",
                source_ref=rf.basename,
                audio=info.as_dict() | {"size": dest.stat().st_size},
            )
            log.info(
                "upgraded %s to %s kbit/s from Soulseek (similarity %.2f)", track.id, info.bitrate, similarity
            )
            return self.store.get_track(track.id)
        finally:
            shutil.rmtree(staging, ignore_errors=True)
            try:
                raw.unlink()
            except OSError:
                pass

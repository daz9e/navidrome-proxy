"""Finding and downloading audio for a catalog track.

Order of preference:

1. **YouTube Music "songs"** – the official audio uploads (``Artist - Topic``); the
   search is a single HTTP call (~0.5 s) instead of spawning ``yt-dlp ytsearch``.
2. **YouTube videos** and **SoundCloud** – searched in parallel only when YouTube
   Music has nothing acceptable.

Every candidate goes through :func:`matching.score_source_candidate`, which hard
rejects wrong durations and unrequested versions (minus/instrumental, karaoke,
sped up, remix, live, cover ...).

yt-dlp runs in-process (as a library, in a worker thread): no interpreter start-up
per track, which alone saved seconds per download on the Raspberry Pi.
"""

from __future__ import annotations

import asyncio
import logging
import threading
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from . import matching

log = logging.getLogger(__name__)

MIN_SCORE = 55.0


@dataclass
class Candidate:
    source: str  # "youtube" | "soundcloud"
    url: str
    ref: str
    title: str
    uploader: str
    duration: float | None
    official: bool = False
    artists: list[str] = field(default_factory=list)
    album: str | None = None
    thumbnail: str | None = None
    score: float = 0.0
    reasons: list[str] = field(default_factory=list)


class SourceError(Exception):
    """Downloading failed; ``transient`` tells the queue whether a retry makes sense."""

    def __init__(self, message: str, transient: bool = True):
        super().__init__(message)
        self.transient = transient


class Cancelled(SourceError):
    def __init__(self, message: str = "cancelled"):
        super().__init__(message, transient=True)


def _ytmusic_search(query: str, filter_: str | None, limit: int) -> list[dict]:
    from ytmusicapi import YTMusic

    return YTMusic().search(query, filter=filter_, limit=limit) or []


def _ytdlp_flat_search(query: str) -> list[dict]:
    import yt_dlp

    opts = {
        "quiet": True,
        "no_warnings": True,
        "extract_flat": "in_playlist",
        "skip_download": True,
        "socket_timeout": 15,
    }
    with yt_dlp.YoutubeDL(opts) as ydl:
        info = ydl.extract_info(query, download=False) or {}
    return [e for e in info.get("entries") or [] if isinstance(e, dict)]


def ytmusic_track(item: dict) -> dict[str, Any] | None:
    """Normalize a YouTube Music search row into a track dict."""
    video_id = item.get("videoId")
    if not video_id:
        return None
    thumbs = item.get("thumbnails") or []
    return {
        "video_id": video_id,
        "title": item.get("title") or video_id,
        "artists": [a.get("name") for a in item.get("artists") or [] if a.get("name")],
        "album": (item.get("album") or {}).get("name"),
        "duration": item.get("duration_seconds"),
        "explicit": bool(item.get("isExplicit")),
        "thumbnail": thumbs[-1]["url"] if thumbs else None,
        "kind": item.get("resultType"),
    }


class Sources:
    def __init__(
        self,
        tolerance: float = 10.0,
        *,
        ytmusic_search: Callable = _ytmusic_search,
        flat_search: Callable = _ytdlp_flat_search,
    ):
        self.tolerance = tolerance
        self._ytmusic_search = ytmusic_search
        self._flat_search = flat_search

    # --------------------------------------------------------------- search
    async def ytmusic(self, query: str, filter_: str | None = "songs", limit: int = 10) -> list[dict]:
        try:
            rows = await asyncio.to_thread(self._ytmusic_search, query, filter_, limit)
        except Exception as exc:
            log.warning("YouTube Music search failed for %r: %s", query, exc)
            return []
        return [t for t in (ytmusic_track(r) for r in rows) if t]

    def _score(self, wanted: matching.Wanted, cand: Candidate) -> Candidate | None:
        scored = matching.score_source_candidate(
            wanted,
            cand.title,
            cand.uploader,
            cand.duration,
            official=cand.official,
            artists=cand.artists,
            tolerance=self.tolerance,
        )
        cand.score, cand.reasons = scored.score, scored.reasons
        if scored.rejected or scored.score < MIN_SCORE:
            log.debug("reject %s %r by %r: %s", cand.source, cand.title, cand.uploader, scored.reasons)
            return None
        return cand

    async def _youtube_music_candidates(self, wanted: matching.Wanted) -> list[Candidate]:
        query = f"{' '.join(wanted.artists)} {wanted.title}".strip()
        out = []
        for t in await self.ytmusic(query, "songs", 10):
            cand = Candidate(
                "youtube",
                f"https://music.youtube.com/watch?v={t['video_id']}",
                t["video_id"],
                t["title"],
                ", ".join(t["artists"]),
                t["duration"],
                official=True,
                artists=t["artists"],
                album=t["album"],
                thumbnail=t["thumbnail"],
            )
            if (scored := self._score(wanted, cand)) is not None:
                out.append(scored)
        return sorted(out, key=lambda c: -c.score)

    async def _flat(self, source: str, prefix: str, wanted: matching.Wanted) -> list[Candidate]:
        query = f"{prefix}:{', '.join(wanted.artists)} - {wanted.title}"
        try:
            entries = await asyncio.to_thread(self._flat_search, query)
        except Exception as exc:
            log.warning("%s search failed for %r: %s", source, query, exc)
            return []
        out = []
        for e in entries:
            if source == "youtube":
                ref = str(e.get("id") or "")
                if len(ref) != 11:
                    continue
                url = f"https://www.youtube.com/watch?v={ref}"
            else:
                url = str(e.get("url") or e.get("webpage_url") or "")
                ref = str(e.get("id") or url)
                if not url:
                    continue
            uploader = str(e.get("channel") or e.get("uploader") or "")
            cand = Candidate(
                source,
                url,
                ref,
                str(e.get("title") or ""),
                uploader,
                e.get("duration"),
                official=uploader.lower().endswith(" - topic"),
            )
            if (scored := self._score(wanted, cand)) is not None:
                out.append(scored)
        return sorted(out, key=lambda c: -c.score)

    async def find(self, wanted: matching.Wanted, exclude: set[str] | None = None) -> list[Candidate]:
        """Acceptable candidates, best first. YouTube Music first; others only if it fails."""
        exclude = exclude or set()
        primary = [c for c in await self._youtube_music_candidates(wanted) if c.ref not in exclude]
        if primary:
            return primary
        youtube, soundcloud = await asyncio.gather(
            self._flat("youtube", "ytsearch8", wanted), self._flat("soundcloud", "scsearch8", wanted)
        )
        rest = [c for c in youtube + soundcloud if c.ref not in exclude]
        return sorted(rest, key=lambda c: -c.score)


# ------------------------------------------------------------------ download
YOUTUBE_FORMAT = "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio"
SOUNDCLOUD_FORMAT = "bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio[ext=mp3]/bestaudio"
PERMANENT_HINTS = (
    "drm",
    "private video",
    "video unavailable",
    "not available",
    "copyright",
    "removed",
    "members-only",
    "sign in to confirm your age",
    "404",
)


def _download_blocking(url: str, fmt: str, dest_dir: Path, cancel: threading.Event, deadline: float) -> Path:
    import yt_dlp

    def hook(progress: dict) -> None:
        if cancel.is_set():
            raise Cancelled()
        if time.monotonic() > deadline:
            raise SourceError("download timed out")

    opts = {
        "format": fmt,
        "outtmpl": str(dest_dir / "source.%(ext)s"),
        "quiet": True,
        "no_warnings": True,
        "noprogress": True,
        "noplaylist": True,
        "socket_timeout": 20,
        "retries": 3,
        "fragment_retries": 3,
        "progress_hooks": [hook],
        "overwrites": True,
        "cachedir": False,
    }
    dest_dir.mkdir(parents=True, exist_ok=True)
    try:
        with yt_dlp.YoutubeDL(opts) as ydl:
            info = ydl.extract_info(url, download=True)
            path = Path(ydl.prepare_filename(info))
    except Cancelled:
        raise
    except SourceError:
        raise
    except Exception as exc:
        text = str(exc)
        permanent = any(hint in text.lower() for hint in PERMANENT_HINTS)
        raise SourceError(f"yt-dlp: {text[-500:]}", transient=not permanent) from exc
    if not path.is_file():
        found = [p for p in dest_dir.iterdir() if p.is_file() and p.name.startswith("source.")]
        if not found:
            raise SourceError("yt-dlp finished without an audio file")
        path = found[0]
    return path


async def download(
    cand: Candidate,
    dest_dir: Path,
    timeout: float = 300.0,
    cancel: threading.Event | None = None,
    downloader: Callable[..., Path] = _download_blocking,
) -> Path:
    """Download ``cand`` into ``dest_dir`` and return the raw file."""
    cancel = cancel or threading.Event()
    fmt = YOUTUBE_FORMAT if cand.source == "youtube" else SOUNDCLOUD_FORMAT
    deadline = time.monotonic() + timeout
    try:
        return await asyncio.to_thread(downloader, cand.url, fmt, dest_dir, cancel, deadline)
    except asyncio.CancelledError:
        cancel.set()
        raise

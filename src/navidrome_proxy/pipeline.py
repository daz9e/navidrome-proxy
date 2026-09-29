"""From an external track id to a playable, tagged file in the library."""

from __future__ import annotations

import asyncio
import logging
import shutil
import threading
from collections.abc import Callable
from pathlib import Path
from typing import Any

from . import audio, ids, lyrics, matching, sources, tagging
from .config import Settings
from .covers import Covers
from .db import Store, Track
from .deezer import Deezer, DeezerError
from .identity import Identity
from .library import Indexer, Library

log = logging.getLogger(__name__)


class Pipeline:
    def __init__(
        self,
        settings: Settings,
        store: Store,
        identity: Identity,
        deezer: Deezer,
        finder: sources.Sources,
        library: Library,
        indexer: Indexer | None,
        covers: Covers,
        downloader: Callable[..., Any] = sources.download,
    ):
        self.settings = settings
        self.store = store
        self.identity = identity
        self.deezer = deezer
        self.finder = finder
        self.library = library
        self.indexer = indexer
        self.covers = covers
        self.downloader = downloader
        self._background: set[asyncio.Task] = set()

    # ---------------------------------------------------------------- helpers
    async def hydrate(self, track: Track) -> dict[str, Any]:
        """Complete catalog metadata for tagging and matching."""
        vid = ids.parse(track.id)
        meta = dict(track.meta)
        if vid is not None and vid.kind == "track" and not meta.get("full"):
            try:
                full = await self.deezer.full_track_meta(vid.key)
                self.store.remember_track(track.id, full)
                meta.update({k: v for k, v in full.items() if v is not None})
            except DeezerError as exc:
                if not (meta.get("title") and meta.get("artist")):
                    raise sources.SourceError(f"Deezer metadata unavailable: {exc}") from exc
                log.info("using search metadata for %s: %s", track.id, exc)
        if not meta.get("title"):
            raise sources.SourceError(f"no metadata for {track.id}", transient=False)
        return meta

    def _direct_candidate(self, track_id: str, meta: dict[str, Any]) -> sources.Candidate | None:
        vid = ids.parse(track_id)
        if vid is None:
            return None
        if vid.kind == "youtube":
            return sources.Candidate(
                "youtube",
                f"https://www.youtube.com/watch?v={vid.key}",
                vid.key,
                meta.get("title", ""),
                meta.get("artist", ""),
                meta.get("duration"),
            )
        if vid.kind == "soundcloud" and meta.get("url"):
            return sources.Candidate(
                "soundcloud",
                meta["url"],
                vid.key,
                meta.get("title", ""),
                meta.get("artist", ""),
                meta.get("duration"),
            )
        return None

    # --------------------------------------------------------------- download
    async def download(self, track_id: str, cancel: threading.Event | None = None) -> Track:
        """Make ``track_id`` playable. Returns the track that now backs it.

        That may be another, already downloaded recording of the same song, in
        which case ``track_id`` becomes an alias of it.
        """
        track = self.store.get_track(track_id)
        if track is None:
            raise sources.SourceError(f"unknown track {track_id}", transient=False)
        if track.is_ready:
            return track
        meta = await self.hydrate(track)
        existing = self.identity.find_existing(meta, exclude=track_id)
        if existing is not None:
            self.identity.link(track_id, existing.id, "recording")
            log.info("%s is the same recording as %s; not downloading again", track_id, existing.id)
            return existing

        direct = self._direct_candidate(track_id, meta)
        wanted = matching.Wanted(
            meta["title"],
            meta.get("artists") or [meta.get("artist", "")],
            duration=meta.get("duration") or None,
        )
        candidates = [direct] if direct else await self.finder.find(wanted)
        if not candidates:
            raise sources.SourceError(
                "no matching audio on YouTube Music, YouTube or SoundCloud", transient=False
            )
        # A found source may itself already be in the library under its own id.
        for cand in candidates:
            source_id = ids.youtube(cand.ref) if cand.source == "youtube" else ids.soundcloud(cand.ref)
            known = self.store.get_track(self.identity.canonical(source_id))
            if known is not None and known.id != track_id and known.is_ready:
                self.identity.link(track_id, known.id, "source")
                return known

        errors: list[sources.SourceError] = []
        for cand in candidates[:3]:
            try:
                return await self._fetch(track, meta, cand, cancel)
            except sources.Cancelled:
                raise
            except sources.SourceError as exc:
                log.warning("%s: %s candidate %s failed: %s", track_id, cand.source, cand.ref, exc)
                errors.append(exc)
        transient = any(e.transient for e in errors)
        raise sources.SourceError("; ".join(str(e) for e in errors)[-800:], transient=transient)

    async def _fetch(
        self, track: Track, meta: dict[str, Any], cand: sources.Candidate, cancel: threading.Event | None
    ) -> Track:
        staging = self.library.new_staging(track.id)
        try:
            raw = await self.downloader(cand, staging, timeout=300, cancel=cancel)
            try:
                prepared, info = await audio.normalize(raw, staging, "prepared")
            except audio.AudioError as exc:
                raise sources.SourceError(str(exc), transient=False) from exc
            expected = float(meta.get("duration") or 0)
            if expected and abs(info.duration - expected) > self.settings.duration_tolerance_seconds:
                raise sources.SourceError(
                    f"duration {info.duration:.0f}s does not match {expected:.0f}s", transient=False
                )
            tag_meta = dict(meta)
            if not tag_meta.get("album") and cand.album and track.id.startswith(("yt:", "sc:")):
                tag_meta["album"] = cand.album
            cover = await self.covers.get(tag_meta.get("cover") or cand.thumbnail)
            await asyncio.to_thread(tagging.write_tags, prepared, tag_meta, cover[0] if cover else None)
            dest = self.library.publish(track.id, prepared, tag_meta, previous=track.file)
            self.store.update_track(
                track.id,
                status="ready",
                file=str(dest),
                source=cand.source,
                source_ref=cand.ref,
                audio=info.as_dict() | {"size": dest.stat().st_size},
                error=None,
            )
            if tag_meta.get("album") != meta.get("album"):
                self.store.remember_track(track.id, {"album": tag_meta["album"]})
            fresh = self.store.get_track(track.id)
            assert fresh is not None
            self.identity.register(fresh)
            if self.indexer is not None:
                self.indexer.notify()
            if self.settings.lyrics:
                self._spawn(self._lyrics(dest, tag_meta))
            log.info(
                "ready %s <- %s %s (%.0fs, %s)", track.id, cand.source, cand.ref, info.duration, dest.name
            )
            return fresh
        finally:
            shutil.rmtree(staging, ignore_errors=True)

    async def _lyrics(self, dest: Path, meta: dict[str, Any]) -> None:
        found = await lyrics.fetch(self.covers.session, meta)
        if found and dest.exists():
            lyrics.save(dest, found)

    def _spawn(self, coro) -> None:
        task = asyncio.create_task(coro)
        self._background.add(task)
        task.add_done_callback(self._background.discard)

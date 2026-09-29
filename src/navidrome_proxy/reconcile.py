"""Link YouTube/SoundCloud-only downloads to the catalog once Deezer knows them.

Only an alias is added (``deezer:<id>`` -> the downloaded track): search results,
albums and likes for the Deezer id then resolve to the existing file. Tags are
left alone on purpose — retagging title/album would give the song a new Navidrome
id and silently drop its likes and play counts.
"""

from __future__ import annotations

import asyncio
import logging
import time

from . import deezer as dz
from . import ids, matching
from .db import Store
from .deezer import Deezer, DeezerError
from .identity import Identity

log = logging.getLogger(__name__)

MIN_SCORE = 85.0


class Reconciler:
    def __init__(
        self, store: Store, identity: Identity, deezer: Deezer, interval: float = 6 * 3600, batch: int = 50
    ):
        self.store = store
        self.identity = identity
        self.deezer = deezer
        self.interval = interval
        self.batch = batch
        self._task: asyncio.Task | None = None

    def start(self) -> None:
        self._task = asyncio.create_task(self._loop(), name="reconciler")

    async def stop(self) -> None:
        if self._task:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass

    async def _loop(self) -> None:
        await asyncio.sleep(120)
        while True:
            try:
                await self.run_once()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("reconcile pass failed")
            await asyncio.sleep(self.interval)

    async def run_once(self) -> int:
        linked = 0
        checked = 0
        for track in self.store.tracks_with_status("ready"):
            if not track.id.startswith(("yt:", "sc:")):
                continue
            if any(alias.startswith("deezer:") for alias, _ in self.store.aliases_of(track.id)):
                continue
            key = f"reconciled:{track.id}"
            if self.store.cache_get(key) is not None:
                continue
            checked += 1
            if checked > self.batch:
                break
            try:
                rows = await self.deezer.search_tracks(f"{' '.join(track.artists[:1])} {track.title}", 10)
            except DeezerError:
                continue
            wanted = matching.Wanted(
                track.title, track.artists, duration=track.audio.get("duration") or track.duration
            )
            best = None
            for row in rows:
                meta = dz.track_meta(row)
                score = matching.score_catalog_candidate(wanted, meta).score
                close = abs(float(meta["duration"] or 0) - float(wanted.duration or 0)) <= 3
                if score >= MIN_SCORE and close and (best is None or score > best[0]):
                    best = (score, row)
            if best is not None:
                deezer_id = ids.track(best[1]["id"])
                if self.identity.link(deezer_id, track.id, "reconciler"):
                    linked += 1
                    log.info("linked %s to catalog %s (score %.0f)", track.id, deezer_id, best[0])
            # Check again in a week (new releases reach Deezer late).
            self.store.cache_set(key, int(time.time()), 7 * 86400)
        return linked

"""Bulk import of plain-text track lists ("Artist - Title" per line).

Each line is matched against the catalog with :func:`matching.score_catalog_candidate`
(title +40, all credited artists +35, album +15, duration +10, ISRC = certain,
version markers penalized). Only candidates crediting at least one of the
requested artists count: a title alone ("No Lie", "Awake") names too many songs.
A confident match is used; otherwise the best credited candidate is still taken
but marked ``fallback`` in the report. Lines Deezer does not know are looked up
on YouTube Music, then SoundCloud (whose matching checks the artist too).

Matched tracks are liked for the importing user and downloaded in the background
lane (they never slow down interactive playback). With ``playlist`` the import
also maintains a Navidrome playlist in the original list order.
"""

from __future__ import annotations

import asyncio
import logging
import re
import time
import uuid
from typing import Any

from . import ids, matching
from .catalog import Catalog
from .db import Store
from .deezer import Deezer, DeezerError
from .identity import Identity
from .navidrome import Navidrome, UpstreamError, UpstreamUnavailable
from .scheduler import BACKGROUND, Scheduler
from .sources import Sources

log = logging.getLogger(__name__)

MAX_LINES = 5000
MAX_LINE = 500
CONFIDENT = 75.0
MARGIN = 5.0

_NUMBER = re.compile(r"^\s*\d+\s*[.)\-:]?\s+")
_DURATION = re.compile(r"\s*[\(\[]\s*(\d{1,2}):(\d{2})\s*[\)\]]\s*$")
_ALBUM = re.compile(r"\s*\[([^\]]+)\]\s*$")
_SEPARATOR = re.compile(r"\s+[-–—]\s+")


def parse_line(raw: str) -> dict[str, Any] | None:
    text = raw.strip().lstrip("﻿")
    if not text:
        return None
    text = _NUMBER.sub("", text, count=1)
    duration = None
    if m := _DURATION.search(text):
        duration = int(m.group(1)) * 60 + int(m.group(2))
        text = text[: m.start()]
    album = None
    if m := _ALBUM.search(text):
        album = m.group(1).strip()
        text = text[: m.start()]
    parts = _SEPARATOR.split(text, maxsplit=1)
    if len(parts) != 2 or not parts[0].strip() or not parts[1].strip():
        return None
    return {"artist": parts[0].strip(), "title": parts[1].strip(), "album": album, "duration": duration}


def parse_text(text: str) -> list[dict[str, Any]]:
    lines = text.splitlines()
    if len(lines) > MAX_LINES:
        raise ValueError(f"at most {MAX_LINES} lines")
    rows = []
    for number, raw in enumerate(lines, 1):
        if not raw.strip():
            continue
        if len(raw) > MAX_LINE:
            rows.append({"line": number, "input": raw[:MAX_LINE], "status": "invalid"})
            continue
        parsed = parse_line(raw)
        if parsed is None:
            rows.append({"line": number, "input": raw, "status": "invalid"})
        else:
            rows.append({"line": number, "input": raw, "status": "pending", **parsed})
    if not rows:
        raise ValueError("no lines to import")
    return rows


class Importer:
    def __init__(
        self,
        store: Store,
        identity: Identity,
        catalog: Catalog,
        deezer: Deezer,
        finder: Sources,
        scheduler: Scheduler,
        navidrome: Navidrome,
    ):
        self.store = store
        self.identity = identity
        self.catalog = catalog
        self.deezer = deezer
        self.finder = finder
        self.scheduler = scheduler
        self.navidrome = navidrome
        self._queue: asyncio.Queue[str] = asyncio.Queue()
        self._task: asyncio.Task | None = None
        self._playlist_task: asyncio.Task | None = None

    def start(self) -> None:
        for imp in self.store.unfinished_imports():
            self._queue.put_nowait(imp["id"])
        self._task = asyncio.create_task(self._worker(), name="imports")
        self._playlist_task = asyncio.create_task(self._playlist_loop(), name="import-playlists")

    async def stop(self) -> None:
        for task in (self._task, self._playlist_task):
            if task:
                task.cancel()
                try:
                    await task
                except asyncio.CancelledError:
                    pass

    # ----------------------------------------------------------------- API
    def create(self, username: str, text: str, like: bool = True, playlist: str | None = None) -> dict:
        rows = parse_text(text)
        import_id = uuid.uuid4().hex[:12]
        options = {"like": like, "playlist": (playlist or "").strip() or None, "playlist_id": None}
        self.store.save_import(import_id, username, "queued", options, rows)
        self._queue.put_nowait(import_id)
        return self.report(import_id)

    def report(self, import_id: str) -> dict:
        imp = self.store.get_import(import_id)
        if imp is None:
            raise KeyError(import_id)
        rows = imp["rows"]
        counts: dict[str, int] = {}
        tracks = self.store.get_tracks(
            self.identity.canonical(r["track_id"]) for r in rows if r.get("track_id")
        )
        for row in rows:
            counts[row["status"]] = counts.get(row["status"], 0) + 1
            if row.get("track_id"):
                track = tracks.get(self.identity.canonical(row["track_id"]))
                row["download"] = track.status if track else "unknown"
                if track is not None and track.error and track.status == "failed":
                    row["download_error"] = track.error[-200:]
        downloads: dict[str, int] = {}
        for row in rows:
            if "download" in row:
                downloads[row["download"]] = downloads.get(row["download"], 0) + 1
        done = sum(1 for r in rows if r["status"] != "pending")
        return {
            "id": imp["id"],
            "status": imp["status"],
            "created_at": imp["created_at"],
            "options": {k: v for k, v in imp["options"].items() if k != "playlist_id"},
            "playlist_id": imp["options"].get("playlist_id"),
            "progress": {"done": done, "total": len(rows)},
            "counts": counts,
            "downloads": downloads,
            "rows": rows,
        }

    def recheck(self, import_id: str, dry_run: bool = False) -> dict[str, Any]:
        """Match again the rows whose catalog match credits none of the requested artists.

        Earlier versions accepted such title-only matches as ``fallback``. The like
        the import set on the wrong song is taken back (as that user, on their next
        request) and the line goes through the current matcher again.
        """
        imp = self.store.get_import(import_id)
        if imp is None:
            raise KeyError(import_id)
        username, rows = imp["username"], imp["rows"]
        stats: dict[str, Any] = {"wrong": 0, "unstar": 0, "lines": []}
        for row in rows:
            match = row.get("match") or {}
            if not row.get("track_id") or not self._wrong_match(row, match):
                continue
            stats["wrong"] += 1
            stats["lines"].append(f"{row['input']}  ->  {match.get('artist')} - {match.get('title')}")
            if dry_run:
                continue
            if not self.store.remove_pending_star(username, row["track_id"]):
                track = self.store.get_track(self.identity.canonical(row["track_id"]))
                if track is not None and track.local_id:
                    self.store.add_pending_unstar(username, track.local_id)
                    stats["unstar"] += 1
            log.info(
                "import %s line %s: %r was matched to %r; matching again",
                import_id,
                row.get("line"),
                row.get("input"),
                f"{match.get('artist')} - {match.get('title')}",
            )
            for key in ("track_id", "match", "alternatives", "download", "download_error"):
                row.pop(key, None)
            row["status"] = "pending"
        if stats["wrong"] and not dry_run:
            self.store.save_import(import_id, username, "queued", imp["options"], rows)
        return stats

    @staticmethod
    def _wrong_match(row: dict[str, Any], match: dict[str, Any]) -> bool:
        artists = matching.split_artists(row["artist"])
        if row["status"] in ("matched", "fallback"):
            return matching.artists_match(artists, match.get("artist") or "") == 0 or not (
                matching.title_matches(row["title"], match.get("title"))
            )
        if row["status"] in ("matched_youtube", "matched_soundcloud"):
            scored = matching.score_source_candidate(
                matching.Wanted(row["title"], artists),
                match.get("title") or "",
                match.get("artist") or "",
                None,
            )
            return scored.rejected is not None
        return False

    # -------------------------------------------------------------- worker
    async def _worker(self) -> None:
        while True:
            import_id = await self._queue.get()
            try:
                await self._run(import_id)
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("import %s crashed", import_id)
                imp = self.store.get_import(import_id)
                if imp:
                    self.store.save_import(import_id, imp["username"], "failed", imp["options"], imp["rows"])

    async def _run(self, import_id: str) -> None:
        from .deezer import background

        background.set(True)
        imp = self.store.get_import(import_id)
        if imp is None or imp["status"] in ("completed", "failed"):
            return
        username, options, rows = imp["username"], imp["options"], imp["rows"]
        self.store.save_import(import_id, username, "processing", options, rows)
        last_save = time.monotonic()
        for row in rows:
            if row["status"] != "pending":
                continue
            try:
                await self._match(row)
            except DeezerError as exc:
                row["status"], row["error"] = "error", str(exc)
            if row.get("track_id"):
                track = self.store.get_track(self.identity.canonical(row["track_id"]))
                if options.get("like"):
                    self.store.add_pending_star(username, row["track_id"])
                if track is not None and not track.is_ready:
                    self.scheduler.request(
                        track.id, BACKGROUND, f"like-import:{username}", fresh=track.status == "failed"
                    )
            if time.monotonic() - last_save > 2:
                self.store.save_import(import_id, username, "processing", options, rows)
                last_save = time.monotonic()
        self.store.save_import(import_id, username, "completed", options, rows)
        log.info("import %s of %s finished: %s", import_id, username, self.report(import_id)["counts"])

    async def _match(self, row: dict[str, Any]) -> None:
        wanted = matching.Wanted(
            row["title"],
            matching.split_artists(row["artist"]),
            album=row.get("album"),
            duration=row.get("duration"),
        )
        query = f"{row['artist']} {row['title']}"
        rows = await self.deezer.search_tracks(query, 15)
        if not rows:
            rows = await self.deezer.search_tracks(row["title"], 15)
        best = await self._rank(wanted, rows) if rows else []
        best = [c for c in best if c["artist_share"] > 0 and matching.title_matches(row["title"], c["title"])]
        if best and best[0]["score"] < 40:
            # Nothing even close in Deezer: another catalog may have the real track.
            others = await self._other_sources(wanted)
            if others:
                row["status"], row["track_id"], row["match"] = (
                    "matched_" + others[0]["source"],
                    others[0]["id"],
                    others[0],
                )
                return
        if best:
            top = best[0]
            confident = top["score"] >= CONFIDENT and (
                len(best) < 2 or top["score"] - best[1]["score"] >= MARGIN or best[1]["id"] == top["id"]
            )
            row["status"] = "matched" if confident else "fallback"
            row["track_id"] = top["id"]
            row["match"] = {
                k: top[k] for k in ("id", "title", "artist", "album", "duration", "score", "reasons")
            }
            row["alternatives"] = [
                {k: c[k] for k in ("id", "title", "artist", "album", "duration", "score")} for c in best[1:4]
            ]
            return
        for cand in await self._other_sources(wanted):
            row["status"], row["track_id"], row["match"] = "matched_" + cand["source"], cand["id"], cand
            return
        row["status"] = "not_found"

    async def _rank(self, wanted: matching.Wanted, rows: list[dict]) -> list[dict]:
        """Score candidates; the top few are re-scored with full credits (collaborations)."""
        self.catalog.remember_deezer(rows)
        from .deezer import track_meta

        first = sorted(
            ((matching.score_catalog_candidate(wanted, track_meta(r)).score, r) for r in rows),
            key=lambda x: -x[0],
        )
        ranked = []
        for _, r in first[:5]:
            try:
                meta = await self.deezer.full_track_meta(r["id"])
                self.store.remember_track(ids.track(r["id"]), meta)
            except DeezerError:
                meta = track_meta(r)
            scored = matching.score_catalog_candidate(wanted, meta)
            ranked.append(
                {
                    "id": ids.track(r["id"]),
                    "title": meta["title"],
                    "artist": meta["artist"],
                    "album": meta.get("album"),
                    "duration": meta.get("duration"),
                    "score": scored.score,
                    "reasons": scored.reasons,
                    "rank": meta.get("rank", 0),
                    "artist_share": matching.artists_match(
                        wanted.artists, meta.get("artists") or meta.get("artist") or ""
                    ),
                }
            )
        ranked.sort(key=lambda c: (-c["score"], -c["rank"]))
        return ranked

    async def _other_sources(self, wanted: matching.Wanted) -> list[dict]:
        found = await self.finder.find(wanted)
        out = []
        for cand in found[:1]:
            if cand.source == "youtube":
                track_id = ids.youtube(cand.ref)
            elif cand.ref.isdigit():
                track_id = ids.soundcloud(cand.ref)
            else:
                continue
            self.store.remember_track(
                track_id,
                {
                    "source": cand.source,
                    "title": wanted.title,
                    "artists": wanted.artists,
                    "artist": ", ".join(wanted.artists),
                    "duration": cand.duration,
                    "album": cand.album,
                    "cover": cand.thumbnail,
                    "url": cand.url,
                },
            )
            out.append(
                {
                    "id": track_id,
                    "source": cand.source,
                    "title": cand.title,
                    "artist": cand.uploader,
                    "duration": cand.duration,
                    "score": cand.score,
                }
            )
        return out

    # ----------------------------------------------------------- playlists
    async def sync_playlists(self) -> None:
        """Keep each import playlist equal to its downloaded tracks, in list order."""
        for import_id in self.store.imports_with_playlist():
            imp = self.store.get_import(import_id)
            if imp is None:
                continue
            creds = self.navidrome.credentials_for(imp["username"])
            if creds is None:
                continue
            local_ids = []
            for row in imp["rows"]:
                if not row.get("track_id"):
                    continue
                track = self.store.get_track(self.identity.canonical(row["track_id"]))
                if (
                    track is not None
                    and track.local_id
                    and track.is_ready
                    and track.local_id not in local_ids
                ):
                    local_ids.append(track.local_id)
            options = imp["options"]
            if options.get("synced") == len(local_ids) and options.get("playlist_id"):
                continue
            try:
                extra = (
                    {"playlistId": options["playlist_id"]}
                    if options.get("playlist_id")
                    else {"name": options["playlist"]}
                )
                root = await self.navidrome.call_many("createPlaylist", creds, {"songId": local_ids}, **extra)
            except (UpstreamError, UpstreamUnavailable) as exc:
                log.warning("import playlist %s not updated: %s", options.get("playlist"), exc)
                continue
            options["playlist_id"] = options.get("playlist_id") or str(
                (root.get("playlist") or {}).get("id") or ""
            )
            options["synced"] = len(local_ids)
            self.store.save_import(imp["id"], imp["username"], imp["status"], options, imp["rows"])

    async def _playlist_loop(self) -> None:
        while True:
            await asyncio.sleep(30)
            try:
                await self.sync_playlists()
            except asyncio.CancelledError:
                raise
            except Exception:
                log.exception("import playlist sync failed")

"""Maintenance commands: adopt the first proxy's library and state, relayout, remux.

``migrate-legacy`` is idempotent and never touches audio files:

1. every library file named with an id token (``deezer-<id>.m4a`` or
   ``... [deezer-<id>].m4a``) becomes a ready track (tags + ffprobe);
2. the legacy ``state.json`` adds Navidrome ids, catalog metadata and per-user
   pending likes; liked tracks that never finished downloading are queued again;
3. pending likes of catalog-less legacy imports (``fallback:*``) become a new
   import for that user, so the better matcher gets a second try.
"""

from __future__ import annotations

import asyncio
import json
import logging
from collections import defaultdict
from pathlib import Path
from typing import Any

from . import audio, ids, matching, tagging
from .covers import Covers, is_complete_image
from .db import Store
from .identity import Identity
from .library import Library, relative_path, track_id_from_path

log = logging.getLogger(__name__)

AUDIO = {".m4a", ".mp3", ".ogg", ".opus", ".flac"}


def _tags_meta(path: Path) -> dict[str, Any]:
    tags = tagging.read_tags(path)

    def first(key: str) -> str | None:
        values = tags.get(key) or []
        return str(values[0]) if values else None

    artist = first("artist") or ""
    artists = [a for a in matching.split_artists(artist.replace(" / ", ", "))] or ([artist] if artist else [])
    track = (first("tracknumber") or "").split("/")[0]
    return {
        "title": first("title") or path.stem,
        "artist": ", ".join(artists) or artist,
        "artists": artists,
        "album": first("album"),
        "album_artist": first("albumartist"),
        "track": int(track) if track.isdigit() else None,
        "year": (first("date") or "")[:4] or None,
        "genre": first("genre"),
    }


def _legacy_meta(job: dict[str, Any]) -> dict[str, Any]:
    artists = job.get("artists") or matching.split_artists(str(job.get("artist") or "").replace(" / ", ", "))
    meta = {
        "title": job.get("title"),
        "artists": artists,
        "artist": ", ".join(artists) if artists else job.get("artist"),
        "album": job.get("album"),
        "album_id": str(job["album_id"]) if job.get("album_id") else None,
        "artist_id": str(job["artist_id"]) if job.get("artist_id") else None,
        "duration": job.get("duration"),
        "cover": job.get("cover"),
    }
    if job.get("id", "").startswith("deezer:"):
        meta["source"] = "deezer"
        meta["deezer_id"] = job["id"].split(":", 1)[1]
    return {k: v for k, v in meta.items() if v not in (None, "", [])}


async def migrate_legacy(
    store: Store,
    music_dir: Path,
    state_file: Path | None,
    legacy_music_root: str = "/music",
    probe_concurrency: int = 4,
) -> dict[str, int]:
    stats: dict[str, int] = defaultdict(int)
    identity = Identity(store)

    # 1. Files already in the library.
    files: dict[str, Path] = {}
    for path in music_dir.rglob("*"):
        if path.suffix.lower() not in AUDIO or ".navidrome-proxy-staging" in path.parts:
            continue
        track_id = track_id_from_path(path)
        if track_id and ids.parse(track_id) is not None:
            files[track_id] = path
    sem = asyncio.Semaphore(probe_concurrency)

    async def adopt(track_id: str, path: Path) -> None:
        existing = store.get_track(track_id)
        if existing is not None and existing.is_ready and existing.file == str(path):
            stats["files_known"] += 1
            return
        async with sem:
            try:
                info = await audio.probe(path)
                meta = await asyncio.to_thread(_tags_meta, path)
            except Exception as exc:
                log.warning("cannot read %s: %s", path, exc)
                stats["files_unreadable"] += 1
                return
        meta["duration"] = round(info.duration)
        store.remember_track(
            track_id, meta if existing is None else {k: v for k, v in meta.items() if k not in existing.meta}
        )
        store.update_track(
            track_id,
            status="ready",
            file=str(path),
            audio=info.as_dict(),
            source=(existing.source if existing else None) or "legacy",
            error=None,
        )
        stats["files_adopted"] += 1

    await asyncio.gather(*(adopt(t, p) for t, p in files.items()))

    if state_file is None or not state_file.is_file():
        return dict(stats)

    # 2. Legacy state.
    jobs: dict[str, dict] = json.loads(state_file.read_text()).get("jobs", {})
    fallback_likes: dict[str, list[str]] = defaultdict(list)
    for key, job in jobs.items():
        if not isinstance(job, dict):
            continue
        vid = ids.parse(key)
        users = list((job.get("pending_star_users") or {}).keys())
        users += [
            r.split(":", 1)[1]
            for r in job.get("storage_reasons") or []
            if isinstance(r, str) and r.startswith("star:") and r.split(":", 1)[1] not in users
        ]
        if key.startswith("fallback:"):
            if job.get("status") != "ready" and job.get("artist") and job.get("title"):
                for user in users:
                    fallback_likes[user].append(f"{job['artist']} - {job['title']}")
            continue
        if vid is None or not vid.is_track:
            continue
        store.remember_track(key, _legacy_meta(job))
        stats["state_tracks"] += 1
        track = store.get_track(key)
        assert track is not None
        if job.get("local_id") and track.is_ready and not track.local_id:
            store.update_track(key, local_id=str(job["local_id"]))
            stats["local_ids"] += 1
        if job.get("status") == "ready" and not track.is_ready and job.get("file"):
            candidate = (
                music_dir / Path(str(job["file"])).relative_to(legacy_music_root)
                if str(job["file"]).startswith(legacy_music_root)
                else Path(str(job["file"]))
            )
            if candidate.is_file():
                await adopt(key, candidate)
        pending_users = list((job.get("pending_star_users") or {}).keys())
        for user in pending_users:
            store.add_pending_star(user, key)
            stats["pending_likes"] += 1
        track = store.get_track(key)
        if pending_users and track is not None and not track.is_ready:
            store.enqueue(key, "download", 2, f"like:{pending_users[0]}")
            stats["requeued_likes"] += 1
        if track is not None and track.is_ready:
            identity.register(track)
    for user, lines in fallback_likes.items():
        from .imports import parse_text

        rows = parse_text("\n".join(dict.fromkeys(lines)))
        import_id = f"legacy-{user.lower()}"
        if store.get_import(import_id) is None:
            store.save_import(
                import_id, user, "queued", {"like": True, "playlist": None, "playlist_id": None}, rows
            )
            stats["legacy_import_rows"] += len(rows)
    return dict(stats)


def relayout(store: Store, music_dir: Path, dry_run: bool = False) -> dict[str, int]:
    """Move adopted files into the ``Artist/Album/NN - Title [id].ext`` layout."""
    library = Library(music_dir, music_dir / ".navidrome-proxy-staging")
    stats = {"moved": 0, "kept": 0, "missing": 0}
    for track in store.tracks_with_status("ready"):
        if not track.file:
            continue
        current = Path(track.file)
        if not current.is_file():
            stats["missing"] += 1
            continue
        target = music_dir / relative_path(track.id, track.meta, current.suffix.lower())
        if target == current:
            stats["kept"] += 1
            continue
        stats["moved"] += 1
        if dry_run:
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        current.replace(target)
        for suffix in (".lrc", ".txt"):
            sidecar = current.with_suffix(suffix)
            if sidecar.exists():
                sidecar.replace(target.with_suffix(suffix))
        store.update_track(track.id, file=str(target))
        library._prune(current.parent)
    return stats


async def remux_dash(music_dir: Path, dry_run: bool = False, concurrency: int = 2) -> dict[str, int]:
    """Losslessly rewrite DASH-fragmented AAC files (wrong duration in AVFoundation)."""
    stats = {"checked": 0, "remuxed": 0, "failed": 0}
    sem = asyncio.Semaphore(concurrency)

    async def one(path: Path) -> None:
        async with sem:
            stats["checked"] += 1
            try:
                if dry_run:
                    info = await audio.probe(path)
                    if info.brand == "dash":
                        stats["remuxed"] += 1
                    return
                if await audio.remux_in_place(path):
                    stats["remuxed"] += 1
            except Exception as exc:
                log.warning("remux of %s failed: %s", path, exc)
                stats["failed"] += 1

    paths = [p for p in music_dir.rglob("*.m4a") if ".navidrome-proxy-staging" not in p.parts]
    await asyncio.gather(*(one(p) for p in paths))
    return stats


async def repair_covers(
    store: Store, music_dir: Path, covers: Covers, dry_run: bool = False
) -> dict[str, int]:
    """Re-embed artwork in files whose cover is truncated (or missing) from the catalog."""
    stats = {"checked": 0, "broken": 0, "repaired": 0, "unresolved": 0}
    for path in sorted(music_dir.rglob("*")):
        if path.suffix.lower() not in (".m4a", ".mp3") or ".navidrome-proxy-staging" in path.parts:
            continue
        stats["checked"] += 1
        try:
            current = await asyncio.to_thread(tagging.embedded_cover, path)
        except Exception as exc:
            log.warning("cannot read %s: %s", path, exc)
            continue
        if current is not None and is_complete_image(current):
            continue
        stats["broken"] += 1
        track_id = track_id_from_path(path)
        track = store.get_track(track_id) if track_id else None
        url = track.meta.get("cover") if track is not None else None
        if not url and track_id and track_id.startswith("yt:"):
            url = f"https://i.ytimg.com/vi/{track_id[3:]}/hqdefault.jpg"
        found = await covers.get(url) if url else None
        if found is None:
            stats["unresolved"] += 1
            log.warning("no cover for %s", path)
            continue
        if not dry_run:
            await asyncio.to_thread(tagging.set_cover, path, found[0])
        stats["repaired"] += 1
    return stats

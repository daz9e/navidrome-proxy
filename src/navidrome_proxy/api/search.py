"""search2 / search3: the local library first, then the external catalog.

Pagination contract (the first proxy broke it): clients page with
``songOffset`` = number of songs received so far and stop at an empty page.
External results are added to the *first* page only; later pages continue the
local results, shifted by the number of external items shown on page one, and
end when the library is exhausted. Arpeggi used to page forever because every
page repeated the same external songs.
"""

from __future__ import annotations

import asyncio
import logging
import re
import time
from typing import Any

from .. import ids, matching
from ..context import Call, Services
from ..deezer import DeezerError
from ..identity import recording_key
from ..navidrome import UpstreamError
from ..subsonic import models
from . import browse, endpoint

log = logging.getLogger(__name__)

YOUTUBE_URL = re.compile(
    r"(?:youtube\.com/(?:watch\?(?:.*&)?v=|shorts/)|youtu\.be/|music\.youtube\.com/watch\?(?:.*&)?v=)"
    r"([A-Za-z0-9_-]{11})"
)
PREFIX = re.compile(r"^(yt|y|youtube|sc|soundcloud)\s*:\s*(.+)$", re.IGNORECASE)

KINDS = ("artist", "album", "song")
_page_memory: dict[tuple[str, str, str], tuple[float, int, int]] = {}


def _result_key(method: str) -> str:
    return "searchResult3" if method == "search3" else "searchResult2"


def _counts(call: Call) -> dict[str, tuple[int, int]]:
    p = call.params
    return {kind: (max(0, p.int(f"{kind}Count", 20)), max(0, p.int(f"{kind}Offset", 0))) for kind in KINDS}


def _as_list(value: Any) -> list:
    if value is None:
        return []
    return value if isinstance(value, list) else [value]


@endpoint("search2", "search3")
async def search(s: Services, call: Call):
    raw_query = call.params.get("query").strip()
    query = raw_query.strip('"').strip()
    if not query or query in ("*", '""'):
        return None  # library sync: pure passthrough
    counts = _counts(call)
    key = _result_key(call.method)
    if all(offset > 0 or count == 0 for count, offset in counts.values()):
        return await _later_page(s, call, query, counts, key)

    local_task = asyncio.create_task(_local(s, call, key))
    external = await _external(s, call, query, counts)
    local = await local_task
    result = await _merge(s, call, query, counts, local, external)
    return call.ok(s.navidrome.identity, {key: result})


async def _local(s: Services, call: Call, key: str, **overrides: str) -> dict[str, list]:
    params = call.params.without("f", "callback")
    extra = {k: v for k, v in params.simple().items() if k not in ("u", "p", "t", "s", "v", "c", "apiKey")}
    extra.update(overrides)
    try:
        root = await s.navidrome.call(call.method, call.params, timeout=30, **extra)
    except UpstreamError as exc:
        log.info("local search failed: %s", exc)
        return {k: [] for k in KINDS}
    result = root.get(key) or {}
    return {k: _as_list(result.get(k)) for k in KINDS}


async def _later_page(s: Services, call: Call, query: str, counts, key: str):
    """Continue local results after the first page (see module docstring)."""
    overrides: dict[str, str] = {}
    empty: set[str] = set()
    for kind, (count, offset) in counts.items():
        if count == 0 or offset == 0:
            continue
        memory = _page_memory.get((call.username, query.casefold(), kind))
        if memory is None:
            continue  # nothing external was shown for this kind: offsets are already local
        _, local_first, external_first = memory
        if local_first < count:
            empty.add(kind)  # the first page already held every local result
        else:
            overrides[f"{kind}Offset"] = str(max(0, offset - external_first))
    local = await _local(s, call, key, **overrides)
    for kind in empty:
        local[kind] = []
    return call.ok(s.navidrome.identity, {key: {k: v for k, v in local.items() if v}})


# ------------------------------------------------------------------ external
async def _external(s: Services, call: Call, query: str, counts) -> dict[str, list]:
    settings = s.settings
    want_songs = counts["song"][0] > 0 and counts["song"][1] == 0
    want_albums = counts["album"][0] > 0 and counts["album"][1] == 0
    want_artists = counts["artist"][0] > 0 and counts["artist"][1] == 0

    video = YOUTUBE_URL.search(query)
    prefix = PREFIX.match(query)
    tasks: dict[str, asyncio.Task] = {}
    if video:
        tasks["youtube"] = asyncio.create_task(_youtube_video(s, video.group(1)))
    elif prefix:
        source, text = prefix.group(1).lower(), prefix.group(2)
        if source.startswith(("y",)):
            tasks["youtube"] = asyncio.create_task(s.finder.ytmusic(text, None, 20))
        else:
            tasks["soundcloud"] = asyncio.create_task(_soundcloud(s, text))
    else:
        if want_songs:
            tasks["tracks"] = asyncio.create_task(s.deezer.search_tracks(query, settings.external_songs))
            if settings.youtube_songs:
                tasks["youtube"] = asyncio.create_task(s.finder.ytmusic(query, "songs", 10))
        if want_albums:
            tasks["albums"] = asyncio.create_task(s.deezer.search_albums(query, settings.external_albums))
        if want_artists:
            tasks["artists"] = asyncio.create_task(s.deezer.search_artists(query, settings.external_artists))
    if not tasks:
        return {}
    done, pending = await asyncio.wait(tasks.values(), timeout=settings.search_budget_seconds)
    for task in pending:
        task.cancel()
    out: dict[str, list] = {}
    for name, task in tasks.items():
        if task in done and not task.cancelled() and task.exception() is None:
            out[name] = task.result() or []
        elif task in done and task.exception() is not None:
            exc = task.exception()
            level = logging.INFO if isinstance(exc, DeezerError) else logging.WARNING
            log.log(level, "external %s search failed for %r: %s", name, query, exc)
        else:
            log.info(
                "external %s search for %r exceeded the %.1fs budget",
                name,
                query,
                settings.search_budget_seconds,
            )
    if prefix or video:
        out["only_external"] = [True]
    return out


async def _youtube_video(s: Services, video_id: str) -> list[dict]:
    rows = await s.finder.ytmusic(video_id, None, 5)
    exact = [r for r in rows if r["video_id"] == video_id]
    if exact:
        return exact[:1]
    return [
        {
            "video_id": video_id,
            "title": video_id,
            "artists": ["YouTube"],
            "duration": None,
            "thumbnail": f"https://i.ytimg.com/vi/{video_id}/hqdefault.jpg",
            "album": None,
        }
    ]


async def _soundcloud(s: Services, text: str) -> list[dict]:
    entries = await asyncio.to_thread(s.finder._flat_search, f"scsearch15:{text}")
    rows = []
    for e in entries:
        ref = str(e.get("id") or "")
        if not ref.isdigit():
            continue
        rows.append(
            {
                "sc_id": ref,
                "url": e.get("url") or e.get("webpage_url"),
                "title": e.get("title") or ref,
                "artists": [e.get("uploader") or "SoundCloud"],
                "duration": e.get("duration"),
                "thumbnail": (e.get("thumbnails") or [{}])[-1].get("url"),
            }
        )
    return rows


def remember_soundcloud(s: Services, rows: list[dict]) -> list[str]:
    items = []
    for row in rows:
        items.append(
            (
                ids.soundcloud(row["sc_id"]),
                {
                    "source": "soundcloud",
                    "title": row["title"],
                    "artists": row["artists"],
                    "artist": ", ".join(row["artists"]),
                    "duration": row.get("duration"),
                    "cover": row.get("thumbnail"),
                    "url": row.get("url"),
                },
            )
        )
    s.store.remember_tracks(items)
    return [track_id for track_id, _ in items]


# --------------------------------------------------------------------- merge
async def _merge(
    s: Services, call: Call, query: str, counts, local: dict[str, list], external: dict[str, list]
) -> dict[str, Any]:
    only_external = bool(external.pop("only_external", None))
    song_count = counts["song"][0]
    local_songs = [] if only_external else local["song"]
    local_ids = {str(x.get("id")) for x in local_songs}
    local_keys = {recording_key(x.get("artist"), x.get("title")) for x in local_songs} - {None}

    deezer_ids = s.catalog.remember_deezer(external.get("tracks", []))
    deezer_keys = {
        recording_key(r.get("artist", {}).get("name"), r.get("title")) for r in external.get("tracks", [])
    } - {None}
    youtube_rows = [
        r
        for r in external.get("youtube", [])
        if only_external or recording_key(r["artists"], r["title"]) not in deezer_keys | local_keys
    ]
    youtube_ids = s.catalog.remember_ytmusic(youtube_rows)
    if not only_external:
        youtube_ids = youtube_ids[: s.settings.youtube_songs]
    soundcloud_ids = remember_soundcloud(s, external.get("soundcloud", []))

    # Local first, then the best catalog hit, then YouTube-only songs, then the rest.
    ordered = deezer_ids[:1] + youtube_ids + soundcloud_ids + deezer_ids[1:]
    presented = await s.catalog.present(ordered, call.params.credentials(), exclude_local=local_ids)
    external_songs = [
        x
        for x in presented
        if str(x.get("id")) not in local_ids
        and (
            ids.parse(str(x.get("id"))) is None
            or recording_key(x.get("artist"), x.get("title")) not in local_keys
        )
    ]
    songs = local_songs + external_songs if song_count else []

    albums = list(local["album"])
    local_album_keys = {
        (matching.normalize(a.get("name")), matching.normalize(a.get("artist"))) for a in albums
    }
    candidate_rows = []
    for row in external.get("albums", []):
        key = (
            matching.normalize(row.get("title")),
            matching.normalize((row.get("artist") or {}).get("name")),
        )
        if key in local_album_keys:
            continue
        local_album_keys.add(key)
        candidate_rows.append(row)
    local_album_ids = {str(a.get("id")) for a in albums}
    external_albums = [
        a
        for a in await browse.merge_with_local(s, call, candidate_rows)
        if str(a.get("id")) not in local_album_ids
    ]
    albums += external_albums

    artists = list(local["artist"])
    names = {matching.normalize(a.get("name")) for a in artists}
    external_artists = []
    for row in external.get("artists", []):
        name = matching.normalize(row.get("name"))
        if not name or name in names:
            continue
        names.add(name)
        external_artists.append(models.artist(ids.artist(row["id"]), row))
    artists += external_artists

    now = time.monotonic()
    for kind, local_items, extra in (
        ("song", local_songs, len(songs) - len(local_songs)),
        ("album", local["album"], len(external_albums)),
        ("artist", local["artist"], len(external_artists)),
    ):
        if extra > 0:
            _page_memory[(call.username, query.casefold(), kind)] = (now, len(local_items), extra)
    if len(_page_memory) > 5000:
        cutoff = now - 1800
        for k in [k for k, v in _page_memory.items() if v[0] < cutoff]:
            _page_memory.pop(k, None)

    result: dict[str, Any] = {}
    if artists and counts["artist"][0]:
        result["artist"] = artists
    if albums and counts["album"][0]:
        result["album"] = albums
    if songs:
        result["song"] = songs
    return result

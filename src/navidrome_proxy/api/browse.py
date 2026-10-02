"""Browsing external entities, and completing local albums/artists with them."""

from __future__ import annotations

import asyncio
import logging
from collections import Counter
from typing import Any

from aiohttp import web

from .. import deezer as dz
from .. import ids, matching
from ..context import Call, Services
from ..deezer import DeezerError
from ..navidrome import UpstreamError
from ..subsonic import models
from ..subsonic import response as sr
from . import endpoint

log = logging.getLogger(__name__)


def _is_json(call: Call) -> bool:
    return call.params.get("f", "xml").lower() in ("json", "jsonp")


def _not_found(s: Services, call: Call, what: str) -> web.Response:
    return call.error(s.navidrome.identity, sr.NOT_FOUND, f"{what} not found")


async def ensure_track(s: Services, track_id: str):
    """The stored track for an external id, fetching unknown Deezer ids on demand."""
    track = s.store.get_track(s.identity.canonical(track_id)) or s.store.get_track(track_id)
    if track is not None:
        return track
    vid = ids.parse(track_id)
    if vid is not None and vid.kind == "track":
        try:
            s.catalog.remember_deezer([await s.deezer.track(vid.key)])
        except DeezerError:
            return None
        return s.store.get_track(track_id)
    return None


# --------------------------------------------------------------------- songs
@endpoint("getSong")
async def get_song(s: Services, call: Call):
    song_id = call.params.get("id")
    vid = ids.parse(song_id)
    if vid is None or not vid.is_track:
        return None
    track = await ensure_track(s, song_id)
    if track is None:
        return _not_found(s, call, "Song")
    starred = s.catalog.starred_map(call.username).get(song_id)
    return call.ok(s.navidrome.identity, {"song": s.catalog.virtual_song(song_id, track, starred)})


# -------------------------------------------------------------------- albums
_lookups = asyncio.Semaphore(2)
DAY = 86400


async def local_album_id(s: Services, deezer_album_id: str, creds) -> str | None:
    """The Navidrome album that already holds tracks of a Deezer album, if any."""
    cache_key = f"local-album:{deezer_album_id}"
    cached = s.store.cache_get(cache_key)
    if cached:
        return cached
    has_local = any(
        (t := s.store.get_track(s.identity.canonical(x.id))) is not None and t.local_id and t.is_ready
        for x in s.store.tracks_in_album(deezer_album_id)
    )
    if not has_local:
        return None
    album_id = await s.intents._local_album(deezer_album_id, creds)
    if album_id:
        s.store.cache_set(cache_key, album_id, 7 * DAY)
        s.store.cache_set(f"deezer-album-for:{album_id}", deezer_album_id, 30 * DAY)
    return album_id


async def local_album_object(s: Services, local_id: str, creds) -> dict[str, Any] | None:
    try:
        root = await s.navidrome.call("getAlbum", creds, id=local_id)
    except UpstreamError:
        return None
    album = root.get("album")
    return album if isinstance(album, dict) else None


def listing(album: dict[str, Any]) -> dict[str, Any]:
    return {k: v for k, v in album.items() if k != "song"}


async def merge_with_local(s: Services, call: Call, rows: list[dict]) -> list[dict[str, Any]]:
    """AlbumID3 entries for Deezer album rows, local albums replacing their catalog twin."""
    creds = call.params.credentials()

    async def one(row: dict) -> dict[str, Any]:
        local_id = await local_album_id(s, str(row["id"]), creds)
        if local_id:
            local = await local_album_object(s, local_id, creds)
            if local is not None:
                return listing(local)
        return models.album(ids.album(row["id"]), row)

    return list(await asyncio.gather(*(one(r) for r in rows)))


def dedupe_editions(albums: list[dict]) -> list[dict]:
    """One entry per album title: the edition with most tracks, explicit preferred."""
    best: dict[str, dict] = {}
    for album in albums:
        key = matching.normalize(album.get("title"))
        rank = (int(album.get("nb_tracks") or 0), bool(album.get("explicit_lyrics")))
        current = best.get(key)
        if current is None or rank > (
            int(current.get("nb_tracks") or 0),
            bool(current.get("explicit_lyrics")),
        ):
            best[key] = album
    return sorted(best.values(), key=lambda a: str(a.get("release_date") or ""), reverse=True)


async def virtual_album(s: Services, call: Call, album_id: str) -> dict[str, Any] | None:
    try:
        data, rows = await asyncio.gather(s.deezer.album(album_id), s.deezer.album_tracks(album_id))
    except DeezerError:
        return None
    track_ids = s.catalog.remember_deezer(rows, album=data)
    songs = await s.catalog.present(track_ids, call.params.credentials())
    starred = s.catalog.starred_map(call.username).get(ids.album(album_id))
    return models.album(ids.album(album_id), data, songs=songs, starred=starred)


@endpoint("getAlbum")
async def get_album(s: Services, call: Call):
    album_id = call.params.get("id")
    vid = ids.parse(album_id)
    if vid is not None:
        if vid.kind != "album":
            return _not_found(s, call, "Album")
        local_id = await local_album_id(s, vid.key, call.params.credentials())
        if local_id:
            local = await local_album_object(s, local_id, call.params.credentials())
            if local is not None:
                await complete_album(s, call, local, deezer_album=vid.key, fetch=True)
                local["id"] = album_id  # keep the id the client asked for
                return call.ok(s.navidrome.identity, {"album": local})
        album = await virtual_album(s, call, vid.key)
        return call.ok(s.navidrome.identity, {"album": album}) if album else _not_found(s, call, "Album")
    if not (s.settings.complete_local_albums and _is_json(call)):
        return None
    try:
        root = await s.navidrome.call("getAlbum", call.params, id=album_id)
    except UpstreamError:
        return None
    album = root.get("album") or {}
    added = await complete_album(s, call, album)
    if not added:
        return None
    return call.ok(s.navidrome.identity, {"album": album})


async def complete_album(
    s: Services, call: Call, album: dict[str, Any], deezer_album: str | None = None, fetch: bool = False
) -> int:
    """Append the catalog tracks a local album is missing, as virtual songs.

    In the request path only cached catalog data is used; a cold album is fetched
    in the background (rate limited) and completed on the next open. That keeps a
    client's full library sync (one getAlbum per album) cheap. Albums that were in
    the library before the proxy are associated with the catalog by name.
    """
    songs = [x for x in album.get("song") or [] if isinstance(x, dict)]
    if deezer_album is None:
        mapped = s.store.tracks_by_local_ids(str(x.get("id")) for x in songs)
        counts = Counter(t.album_id for t in mapped.values() if t.album_id)
        if counts:
            deezer_album = counts.most_common(1)[0][0]
        else:
            deezer_album = s.store.cache_get(f"deezer-album-for:{album.get('id')}")
            if deezer_album is None:
                s.background.spawn(
                    _associate(s, str(album.get("id")), album.get("name"), album.get("artist"))
                )
                return 0
            if not deezer_album:
                return 0  # known to have no catalog twin
    rows = s.deezer.cached_album_tracks(deezer_album)
    if rows is None:
        if not fetch:
            s.background.spawn(_warm_album(s, deezer_album))
            return 0
        try:
            await s.deezer.album(deezer_album)
            rows = await s.deezer.album_tracks(deezer_album)
        except DeezerError:
            return 0
    data = s.deezer.cached_album(deezer_album) or {}
    track_ids = s.catalog.remember_deezer(rows, album=data or None)
    canonical = s.identity.canonical_many(track_ids)
    have_local = {str(x.get("id")) for x in songs}
    have_titles = {matching.base_title(x.get("title")) for x in songs}
    tracks = s.store.get_tracks(set(canonical.values()))
    missing = []
    for track_id in track_ids:
        track = tracks.get(canonical[track_id])
        if track is None:
            continue
        if track.local_id in have_local or matching.base_title(track.title) in have_titles:
            continue
        missing.append(track_id)
    if not missing:
        return 0
    extra = await s.catalog.present(missing, call.params.credentials(), exclude_local=have_local)
    merged = songs + extra
    merged.sort(key=lambda x: (int(x.get("discNumber") or 1), int(x.get("track") or 0)))
    album["song"] = merged
    album["songCount"] = len(merged)
    album["duration"] = sum(int(x.get("duration") or 0) for x in merged)
    return len(extra)


async def _associate(s: Services, local_album: str, name: str | None, artist: str | None) -> None:
    """Find the catalog twin of a local album by exact title and artist (cached, also negatives)."""
    key = f"deezer-album-for:{local_album}"
    if not name or s.store.cache_get(key) is not None:
        return
    async with _lookups:
        try:
            rows = await s.deezer.search_albums(f"{artist or ''} {name}".strip(), 10)
        except DeezerError:
            return
        found = ""
        for row in rows:
            if (
                matching.normalize(row.get("title")) == matching.normalize(name)
                and matching.artists_match([artist or ""], (row.get("artist") or {}).get("name") or "") == 1
            ):
                found = str(row["id"])
                break
        s.store.cache_set(key, found, 30 * DAY)
        if found:
            await _warm_album(s, found)


async def _warm_album(s: Services, album_id: str) -> None:
    try:
        await s.deezer.album(album_id)
        await s.deezer.album_tracks(album_id)
    except DeezerError as exc:
        log.debug("album %s warm-up failed: %s", album_id, exc)


# ------------------------------------------------------------------- artists
async def virtual_artist(s: Services, call: Call, artist_id: str) -> dict[str, Any] | None:
    try:
        data, albums = await asyncio.gather(s.deezer.artist(artist_id), s.deezer.artist_albums(artist_id))
    except DeezerError:
        return None
    listed = await merge_with_local(
        s, call, [{**a, "artist": a.get("artist") or data} for a in dedupe_editions(albums)]
    )
    starred = s.catalog.starred_map(call.username).get(ids.artist(artist_id))
    return models.artist(ids.artist(artist_id), data, albums=listed, starred=starred)


@endpoint("getArtist")
async def get_artist(s: Services, call: Call):
    artist_id = call.params.get("id")
    vid = ids.parse(artist_id)
    if vid is not None:
        if vid.kind != "artist":
            return _not_found(s, call, "Artist")
        artist = await virtual_artist(s, call, vid.key)
        return call.ok(s.navidrome.identity, {"artist": artist}) if artist else _not_found(s, call, "Artist")
    if not (s.settings.complete_local_albums and _is_json(call)):
        return None
    try:
        root = await s.navidrome.call("getArtist", call.params, id=artist_id)
    except UpstreamError:
        return None
    artist = root.get("artist") or {}
    try:
        added = await asyncio.wait_for(complete_artist(s, artist), timeout=3.0)
    except (TimeoutError, DeezerError):
        added = 0
    if not added:
        return None
    return call.ok(s.navidrome.identity, {"artist": artist})


async def find_deezer_artist(s: Services, name: str) -> dict | None:
    wanted = matching.normalize(name)
    if not wanted:
        return None
    for row in await s.deezer.search_artists(name, 5):
        if matching.normalize(row.get("name")) == wanted:
            return row
    return None


async def complete_artist(s: Services, artist: dict[str, Any]) -> int:
    """Append catalog albums a local artist does not have yet."""
    match = await find_deezer_artist(s, artist.get("name") or "")
    if match is None:
        return 0
    local_albums = [a for a in artist.get("album") or [] if isinstance(a, dict)]
    have = {matching.normalize(a.get("name")) for a in local_albums}
    extra = []
    for row in dedupe_editions(await s.deezer.artist_albums(match["id"])):
        if matching.normalize(row.get("title")) in have:
            continue
        extra.append(models.album(ids.album(row["id"]), {**row, "artist": row.get("artist") or match}))
    if not extra:
        return 0
    artist["album"] = local_albums + extra
    artist["albumCount"] = len(artist["album"])
    return len(extra)


def _images(item: dict, kind: str = "picture") -> dict[str, str]:
    md5 = item.get("md5_image")
    image_kind = "artist" if kind == "picture" else "cover"
    return {
        "smallImageUrl": dz.cover_url(md5, 250, image_kind) or item.get(f"{kind}_medium") or "",
        "mediumImageUrl": dz.cover_url(md5, 500, image_kind) or item.get(f"{kind}_big") or "",
        "largeImageUrl": dz.cover_url(md5, 1000, image_kind) or item.get(f"{kind}_xl") or "",
    }


@endpoint("getArtistInfo", "getArtistInfo2")
async def get_artist_info(s: Services, call: Call):
    vid = ids.parse(call.params.get("id"))
    if vid is None:
        return None
    key = "artistInfo2" if call.method == "getArtistInfo2" else "artistInfo"
    if vid.kind != "artist":
        return call.ok(s.navidrome.identity, {key: {}})
    try:
        data = await s.deezer.artist(vid.key)
        related = await s.deezer.artist_related(vid.key, call.params.int("count", 20))
    except DeezerError:
        return call.ok(s.navidrome.identity, {key: {}})
    info = {
        "biography": "",
        "musicBrainzId": "",
        "lastFmUrl": "",
        **_images(data),
        "similarArtist": [models.artist(ids.artist(a["id"]), a) for a in related],
    }
    return call.ok(s.navidrome.identity, {key: info})


@endpoint("getAlbumInfo", "getAlbumInfo2")
async def get_album_info(s: Services, call: Call):
    vid = ids.parse(call.params.get("id"))
    if vid is None:
        return None
    key = "albumInfo"
    if vid.kind != "album":
        return call.ok(s.navidrome.identity, {key: {}})
    try:
        data = await s.deezer.album(vid.key)
    except DeezerError:
        return call.ok(s.navidrome.identity, {key: {}})
    return call.ok(
        s.navidrome.identity,
        {key: {"notes": "", "musicBrainzId": "", "lastFmUrl": "", **_images(data, "cover")}},
    )


@endpoint("getTopSongs")
async def get_top_songs(s: Services, call: Call):
    name = call.params.get("artist")
    count = call.params.int("count", 50)
    try:
        root = await s.navidrome.call("getTopSongs", call.params, artist=name, count=str(count))
        local = (root.get("topSongs") or {}).get("song") or []
    except UpstreamError:
        local = []
    if local:
        return None
    try:
        match = await find_deezer_artist(s, name)
        rows = await s.deezer.artist_top(match["id"], min(count, 50)) if match else []
    except DeezerError:
        rows = []
    track_ids = s.catalog.remember_deezer(rows)
    songs = await s.catalog.present(track_ids, call.params.credentials())
    return call.ok(s.navidrome.identity, {"topSongs": {"song": songs} if songs else {}})


@endpoint("getSimilarSongs", "getSimilarSongs2")
async def get_similar_songs(s: Services, call: Call):
    vid = ids.parse(call.params.get("id"))
    if vid is None:
        return None
    key = "similarSongs2" if call.method == "getSimilarSongs2" else "similarSongs"
    rows: list[dict] = []
    try:
        if vid.kind == "artist":
            rows = await s.deezer.artist_top(vid.key, 10)
            for related in (await s.deezer.artist_related(vid.key, 4))[:4]:
                rows += await s.deezer.artist_top(related["id"], 5)
    except DeezerError:
        rows = []
    track_ids = s.catalog.remember_deezer(rows)
    songs = await s.catalog.present(track_ids, call.params.credentials())
    return call.ok(s.navidrome.identity, {key: {"song": songs} if songs else {}})


# --------------------------------------------------------------------- media
def _sized(url: str | None, size: int) -> str | None:
    if not url:
        return None
    for known in (1000, 500, 250):
        token = f"/{known}x{known}-"
        if token in url:
            target = 250 if size and size <= 250 else 500 if size and size <= 500 else 1000
            return url.replace(token, f"/{target}x{target}-")
    return url


@endpoint("getCoverArt")
async def get_cover_art(s: Services, call: Call):
    art_id = call.params.get("id")
    vid = ids.parse(art_id)
    if vid is None:
        return None
    size = call.params.int("size", 0)
    url: str | None = None
    try:
        if vid.is_track:
            track = await ensure_track(s, art_id)
            url = track.meta.get("cover") if track is not None else None
            if not url and vid.kind == "youtube":
                url = f"https://i.ytimg.com/vi/{vid.key}/hqdefault.jpg"
        elif vid.kind == "album":
            url = dz.best_cover(await s.deezer.album(vid.key))
        elif vid.kind == "artist":
            data = await s.deezer.artist(vid.key)
            url = dz.cover_url(data.get("md5_image"), 1000, "artist") or data.get("picture_xl")
    except DeezerError:
        url = None
    found = await s.covers.get(_sized(url, size))
    if found is None:
        return web.Response(status=404, text="cover not found")
    data, content_type = found
    return web.Response(
        body=data, content_type=content_type, headers={"Cache-Control": "public, max-age=604800"}
    )


@endpoint("getLyricsBySongId")
async def get_lyrics_by_song_id(s: Services, call: Call):
    vid = ids.parse(call.params.get("id"))
    if vid is None:
        return None
    track = s.store.get_track(s.identity.canonical(call.params.get("id")))
    if track is not None and track.local_id and track.is_ready:
        try:
            root = await s.navidrome.call("getLyricsBySongId", call.params, id=track.local_id)
            return call.ok(s.navidrome.identity, {"lyricsList": root.get("lyricsList") or {}})
        except UpstreamError:
            pass
    return call.ok(s.navidrome.identity, {"lyricsList": {}})

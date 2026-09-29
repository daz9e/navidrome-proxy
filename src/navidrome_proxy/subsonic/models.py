"""Virtual Subsonic entities shaped exactly like Navidrome 0.63 objects.

Strict clients decode responses with typed models (Arpeggi and Resonus use Swift
``Codable``): a missing key, a ``null`` or a boolean where a date string belongs
crashes them. Every builder here therefore emits the same key set and JSON types
as the real Navidrome object, captured from a live server.
"""

from __future__ import annotations

from datetime import UTC, datetime
from typing import Any

from .. import ids

CONTENT_TYPES = {
    "m4a": "audio/mp4",
    "mp4": "audio/mp4",
    "mp3": "audio/mpeg",
    "opus": "audio/ogg",
    "ogg": "audio/ogg",
    "flac": "audio/flac",
}


def iso(ts: float | int | str | None) -> str:
    """Navidrome's timestamp format; never empty."""
    if isinstance(ts, str) and ts:
        try:
            parsed = datetime.fromisoformat(ts.replace("Z", "+00:00"))
            if parsed.tzinfo is None:
                parsed = parsed.replace(tzinfo=UTC)
            return parsed.astimezone(UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        except ValueError:
            pass
    try:
        value = float(ts) if ts not in (None, "") else datetime.now(UTC).timestamp()
    except (TypeError, ValueError):
        value = datetime.now(UTC).timestamp()
    return datetime.fromtimestamp(value, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def _int(value: Any) -> int:
    try:
        return max(0, int(float(value)))
    except (TypeError, ValueError, OverflowError):
        return 0


def _str(value: Any) -> str:
    return "" if value is None else str(value)


def item_date(date: str | None) -> dict[str, int]:
    """OpenSubsonic ItemDate: only the known parts, like Navidrome."""
    parts = [p for p in _str(date).split("-") if p.isdigit()]
    out: dict[str, int] = {}
    for key, part in zip(("year", "month", "day"), parts, strict=False):
        if int(part):
            out[key] = int(part)
    return out


def _artist_refs(names: list[str], artist_id: str) -> list[dict[str, str]]:
    refs = []
    for index, name in enumerate(names):
        refs.append({"id": artist_id if index == 0 and artist_id else "", "name": name})
    return refs


def song(
    track_id: str,
    meta: dict[str, Any],
    *,
    audio: dict[str, Any] | None = None,
    created: float | str | None = None,
    starred: float | None = None,
) -> dict[str, Any]:
    """A virtual song (``Child``) for ``deezer:`` and ``yt:`` tracks."""
    audio = audio or {}
    album_id = ids.album(meta["album_id"]) if meta.get("album_id") else ""
    artist_id = ids.artist(meta["artist_id"]) if meta.get("artist_id") else ""
    names = [n for n in (meta.get("artists") or [meta.get("artist")]) if n] or ["Unknown Artist"]
    display = _str(meta.get("artist")) or ", ".join(names)
    album_artist = _str(meta.get("album_artist")) or names[0]
    suffix = _str(audio.get("suffix")) or "m4a"
    vid = ids.parse(track_id)
    cover = album_id or track_id
    if vid is not None and vid.kind == "youtube":
        cover = track_id
    duration = audio.get("duration") or meta.get("duration")
    out: dict[str, Any] = {
        "id": track_id,
        "parent": album_id,
        "isDir": False,
        "title": _str(meta.get("title")) or track_id,
        "album": _str(meta.get("album")),
        "artist": display,
        "track": _int(meta.get("track")),
        "year": _int(meta.get("year")),
        "genre": _str(meta.get("genre")),
        "coverArt": cover,
        "size": _int(audio.get("size")),
        "contentType": CONTENT_TYPES.get(suffix, "audio/mp4"),
        "suffix": suffix,
        "duration": _int(round(float(duration or 0))),
        "bitRate": _int(audio.get("bitrate")),
        "path": "",
        "discNumber": _int(meta.get("disc")) or 1,
        "created": iso(created),
        "albumId": album_id,
        "artistId": artist_id,
        "type": "music",
        "bpm": 0,
        "comment": "",
        "sortName": _str(meta.get("title")).lower(),
        "mediaType": "song",
        "musicBrainzId": "",
        "isrc": [meta["isrc"]] if meta.get("isrc") else [],
        "genres": [{"name": meta["genre"]}] if meta.get("genre") else [],
        "replayGain": {},
        "channelCount": _int(audio.get("channels")) or 2,
        "samplingRate": _int(audio.get("sample_rate")) or 44100,
        "bitDepth": 0,
        "moods": [],
        "artists": _artist_refs(names, artist_id),
        "displayArtist": display,
        "albumArtists": _artist_refs([album_artist], artist_id),
        "displayAlbumArtist": album_artist,
        "contributors": [],
        "displayComposer": "",
        "explicitStatus": "explicit" if meta.get("explicit") else "",
        "groupings": [],
        "works": [],
        "movements": [],
    }
    if starred:
        out["starred"] = iso(starred)
    return out


def album(
    album_id: str,
    data: dict[str, Any],
    *,
    songs: list[dict[str, Any]] | None = None,
    starred: float | None = None,
) -> dict[str, Any]:
    """A virtual album (``AlbumID3``) from a Deezer album object."""
    artist = data.get("artist") or {}
    artist_id = ids.artist(artist["id"]) if artist.get("id") else ""
    name = _str(data.get("title"))
    release = _str(data.get("release_date"))
    genres = [g.get("name") for g in (data.get("genres") or {}).get("data", []) if g.get("name")]
    record_type = _str(data.get("record_type"))
    duration = data.get("duration")
    if not duration and songs:
        duration = sum(s.get("duration", 0) for s in songs)
    out: dict[str, Any] = {
        "id": album_id,
        "name": name,
        "artist": _str(artist.get("name")),
        "artistId": artist_id,
        "coverArt": album_id,
        "songCount": len(songs) if songs is not None else _int(data.get("nb_tracks")),
        "duration": _int(duration),
        "created": iso(release or None),
        "year": _int(release[:4]),
        "genre": genres[0] if genres else "",
        "userRating": 0,
        "genres": [{"name": g} for g in genres],
        "musicBrainzId": "",
        "isCompilation": record_type == "compile",
        "sortName": name.lower(),
        "discTitles": [],
        "originalReleaseDate": {},
        "releaseDate": item_date(release),
        "releaseTypes": [record_type] if record_type else [],
        "recordLabels": [{"name": data["label"]}] if data.get("label") else [],
        "moods": [],
        "artists": [{"id": artist_id, "name": _str(artist.get("name"))}] if artist.get("name") else [],
        "displayArtist": _str(artist.get("name")),
        "explicitStatus": "explicit" if data.get("explicit_lyrics") else "",
        "version": "",
    }
    if starred:
        out["starred"] = iso(starred)
    if songs is not None:
        out["song"] = songs
    return out


def artist(
    artist_id: str,
    data: dict[str, Any],
    *,
    albums: list[dict[str, Any]] | None = None,
    starred: float | None = None,
) -> dict[str, Any]:
    """A virtual artist (``ArtistID3``) from a Deezer artist object."""
    name = _str(data.get("name"))
    out: dict[str, Any] = {
        "id": artist_id,
        "name": name,
        "coverArt": artist_id,
        "albumCount": len(albums) if albums is not None else _int(data.get("nb_album")),
        "artistImageUrl": _str(data.get("picture_xl") or data.get("picture_big") or data.get("picture")),
        "musicBrainzId": "",
        "sortName": name.lower(),
        "roles": ["artist", "albumartist"],
    }
    if starred:
        out["starred"] = iso(starred)
    if albums is not None:
        out["album"] = albums
    return out

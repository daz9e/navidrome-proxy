"""Canonical tags for imported files.

Files arrive with all foreign metadata stripped (see :func:`audio.normalize`), so
only the catalog's own values end up in the library. That matters for Navidrome's
album grouping: stray release dates, MusicBrainz ids or "comment=youtube" tags
split one album into several in the first proxy generation.

``ARTIST`` carries the display credit ("TVETH, Bato"), the multi-valued
``ARTISTS`` tag carries each performer, so Navidrome lists the track under every
artist of a collaboration.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import mutagen
from mutagen.id3 import APIC, TALB, TCON, TDRC, TIT2, TPE1, TPE2, TPOS, TRCK, TSRC, TXXX
from mutagen.mp4 import MP4, MP4Cover, MP4FreeForm


def _text(meta: dict[str, Any], key: str) -> str:
    value = meta.get(key)
    return "" if value is None else str(value)


def write_tags(path: Path, meta: dict[str, Any], cover: bytes | None = None) -> None:
    title = _text(meta, "title")
    artists = [str(a) for a in meta.get("artists") or [] if a] or (
        [meta["artist"]] if meta.get("artist") else []
    )
    display = _text(meta, "artist") or ", ".join(artists)
    album = _text(meta, "album")
    album_artist = _text(meta, "album_artist") or (artists[0] if artists else "")
    track = int(meta.get("track") or 0)
    total = int(meta.get("track_total") or 0)
    disc = int(meta.get("disc") or 1)
    year = str(meta.get("year") or "")[:4]
    genre = _text(meta, "genre")
    isrc = _text(meta, "isrc")
    suffix = path.suffix.lower()

    if suffix == ".m4a":
        audio = MP4(str(path))
        if audio.tags is None:
            audio.add_tags()
        tags = audio.tags
        tags.clear()
        tags["\xa9nam"] = [title]
        tags["\xa9ART"] = [display]
        if artists:
            tags["----:com.apple.iTunes:ARTISTS"] = [MP4FreeForm(a.encode()) for a in artists]
        if album:
            tags["\xa9alb"] = [album]
            tags["aART"] = [album_artist]
        if track:
            tags["trkn"] = [(track, total)]
        tags["disk"] = [(disc, 0)]
        if year:
            tags["\xa9day"] = [year]
        if genre:
            tags["\xa9gen"] = [genre]
        if isrc:
            tags["----:com.apple.iTunes:ISRC"] = [MP4FreeForm(isrc.encode())]
        if meta.get("explicit"):
            tags["rtng"] = [1]
        if cover:
            fmt = MP4Cover.FORMAT_PNG if cover.startswith(b"\x89PNG") else MP4Cover.FORMAT_JPEG
            tags["covr"] = [MP4Cover(cover, imageformat=fmt)]
        audio.save()
        return

    if suffix == ".mp3":
        audio = mutagen.File(str(path))
        if audio is None:
            raise ValueError(f"unsupported file {path}")
        if audio.tags is None:
            audio.add_tags()
        tags = audio.tags
        tags.clear()
        tags.add(TIT2(encoding=3, text=[title]))
        tags.add(TPE1(encoding=3, text=[display]))
        if artists:
            tags.add(TXXX(encoding=3, desc="ARTISTS", text=artists))
        if album:
            tags.add(TALB(encoding=3, text=[album]))
            tags.add(TPE2(encoding=3, text=[album_artist]))
        if track:
            tags.add(TRCK(encoding=3, text=[f"{track}/{total}" if total else str(track)]))
        tags.add(TPOS(encoding=3, text=[str(disc)]))
        if year:
            tags.add(TDRC(encoding=3, text=[year]))
        if genre:
            tags.add(TCON(encoding=3, text=[genre]))
        if isrc:
            tags.add(TSRC(encoding=3, text=[isrc]))
        if cover:
            mime = "image/png" if cover.startswith(b"\x89PNG") else "image/jpeg"
            tags.add(APIC(encoding=3, mime=mime, type=3, desc="Cover", data=cover))
        audio.save(v2_version=3)
        return
    raise ValueError(f"cannot tag {suffix} files")


def read_tags(path: Path) -> dict[str, Any]:
    """Easy-mode tag view, used by tests and the migration tool."""
    audio = mutagen.File(str(path), easy=True)
    if audio is None or audio.tags is None:
        return {}
    return {k: list(v) for k, v in audio.tags.items()}

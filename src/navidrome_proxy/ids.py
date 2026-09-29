"""Virtual identifiers for entities that exist outside the local Navidrome library.

The scheme is kept identical to the first proxy generation so client caches,
queued likes and stored mappings survive the rewrite:

* ``deezer:<track id>``         – a Deezer track
* ``deezer-album:<album id>``   – a Deezer album
* ``deezer-artist:<artist id>`` – a Deezer artist
* ``yt:<video id>``             – a YouTube video used as a standalone track
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Literal

Kind = Literal["track", "album", "artist", "youtube"]

_YOUTUBE_ID = re.compile(r"^[A-Za-z0-9_-]{11}$")
_NUMERIC = re.compile(r"^\d{1,20}$")

_PREFIXES: dict[str, Kind] = {
    "deezer:": "track",
    "deezer-album:": "album",
    "deezer-artist:": "artist",
    "yt:": "youtube",
}


@dataclass(frozen=True)
class VirtualId:
    kind: Kind
    key: str

    def __str__(self) -> str:
        for prefix, kind in _PREFIXES.items():
            if kind == self.kind:
                return prefix + self.key
        raise AssertionError(self.kind)

    @property
    def is_track(self) -> bool:
        return self.kind in ("track", "youtube")


def parse(value: str | None) -> VirtualId | None:
    """Return the virtual id encoded in ``value`` or ``None`` for a Navidrome id."""
    if not value:
        return None
    # "deezer-album:" and "deezer-artist:" must be tested before "deezer:".
    for prefix in ("deezer-album:", "deezer-artist:", "deezer:", "yt:"):
        if value.startswith(prefix):
            key = value[len(prefix) :]
            kind = _PREFIXES[prefix]
            valid = _YOUTUBE_ID.match(key) if kind == "youtube" else _NUMERIC.match(key)
            return VirtualId(kind, key) if valid else None
    return None


def is_virtual(value: str | None) -> bool:
    return parse(value) is not None


def track(deezer_id: int | str) -> str:
    return f"deezer:{deezer_id}"


def album(deezer_id: int | str) -> str:
    return f"deezer-album:{deezer_id}"


def artist(deezer_id: int | str) -> str:
    return f"deezer-artist:{deezer_id}"


def youtube(video_id: str) -> str:
    return f"yt:{video_id}"


def valid_youtube_id(video_id: str) -> bool:
    return bool(_YOUTUBE_ID.match(video_id or ""))

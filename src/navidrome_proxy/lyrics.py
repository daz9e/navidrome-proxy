"""Lyrics from lrclib.net, stored next to the track where Navidrome picks them up."""

from __future__ import annotations

import logging
from pathlib import Path
from typing import Any

import aiohttp

log = logging.getLogger(__name__)

LRCLIB = "https://lrclib.net/api/get"


async def fetch(
    session: aiohttp.ClientSession, meta: dict[str, Any], base_url: str = LRCLIB
) -> tuple[str, str] | None:
    """Return ``("lrc"|"txt", text)`` or None."""
    params = {
        "artist_name": (meta.get("artists") or [meta.get("artist") or ""])[0],
        "track_name": meta.get("title") or "",
        "album_name": meta.get("album") or "",
        "duration": str(int(float(meta.get("duration") or 0))),
    }
    try:
        async with session.get(
            base_url,
            params=params,
            timeout=aiohttp.ClientTimeout(total=15),
            headers={"User-Agent": "navidrome-proxy/2 (https://github.com)"},
        ) as resp:
            if resp.status != 200:
                return None
            data = await resp.json(content_type=None)
    except Exception as exc:
        log.debug("lyrics lookup failed for %s: %s", meta.get("title"), exc)
        return None
    if not isinstance(data, dict) or data.get("instrumental"):
        return None
    if data.get("syncedLyrics"):
        return "lrc", data["syncedLyrics"]
    if data.get("plainLyrics"):
        return "txt", data["plainLyrics"]
    return None


def save(audio_path: Path, found: tuple[str, str]) -> Path:
    kind, text = found
    target = audio_path.with_suffix(f".{kind}")
    target.write_text(text, encoding="utf-8")
    return target

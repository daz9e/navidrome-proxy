"""Artwork fetched from the catalog, cached on disk."""

from __future__ import annotations

import asyncio
import hashlib
import logging
from pathlib import Path

import aiohttp

log = logging.getLogger(__name__)

MAX_BYTES = 8 * 1024 * 1024


class Covers:
    def __init__(self, cache_dir: Path):
        self.cache_dir = cache_dir
        self._session: aiohttp.ClientSession | None = None
        self._locks: dict[str, asyncio.Lock] = {}

    async def start(self) -> None:
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self._session = aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=20, connect=5))

    async def close(self) -> None:
        if self._session:
            await self._session.close()

    @property
    def session(self) -> aiohttp.ClientSession:
        assert self._session is not None, "Covers.start() was not awaited"
        return self._session

    def _path(self, url: str) -> Path:
        digest = hashlib.sha256(url.encode()).hexdigest()
        return self.cache_dir / digest[:2] / f"{digest}.img"

    async def get(self, url: str | None) -> tuple[bytes, str] | None:
        """Image bytes and content type, or None when unavailable."""
        if not url or not url.startswith(("http://", "https://")):
            return None
        path = self._path(url)
        lock = self._locks.setdefault(url, asyncio.Lock())
        async with lock:
            if path.is_file():
                data = path.read_bytes()
                return data, _content_type(data)
            try:
                async with self.session.get(url) as resp:
                    if resp.status != 200:
                        return None
                    data = await resp.content.read(MAX_BYTES + 1)
            except (aiohttp.ClientError, TimeoutError) as exc:
                log.debug("cover %s unavailable: %s", url, exc)
                return None
            finally:
                self._locks.pop(url, None)
            if not data or len(data) > MAX_BYTES:
                return None
            path.parent.mkdir(parents=True, exist_ok=True)
            tmp = path.with_suffix(".tmp")
            tmp.write_bytes(data)
            tmp.replace(path)
            return data, _content_type(data)


def _content_type(data: bytes) -> str:
    if data.startswith(b"\x89PNG"):
        return "image/png"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    return "image/jpeg"

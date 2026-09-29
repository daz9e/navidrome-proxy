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
                if is_complete_image(data):
                    return data, _content_type(data)
                path.unlink(missing_ok=True)  # a truncated download cached by an older version
            try:
                data = await self._download(url)
            except (aiohttp.ClientError, TimeoutError) as exc:
                log.debug("cover %s unavailable: %s", url, exc)
                return None
            finally:
                self._locks.pop(url, None)
            if data is None:
                return None
            path.parent.mkdir(parents=True, exist_ok=True)
            tmp = path.with_suffix(".tmp")
            tmp.write_bytes(data)
            tmp.replace(path)
            return data, _content_type(data)

    async def _download(self, url: str) -> bytes | None:
        async with self.session.get(url) as resp:
            if resp.status != 200:
                return None
            chunks: list[bytes] = []
            size = 0
            async for chunk in resp.content.iter_chunked(64 * 1024):
                size += len(chunk)
                if size > MAX_BYTES:
                    return None
                chunks.append(chunk)
            data = b"".join(chunks)
            if resp.content_length is not None and len(data) != resp.content_length:
                return None
        if not is_complete_image(data):
            log.warning("cover %s is not a complete image (%d bytes)", url, len(data))
            return None
        return data


def is_complete_image(data: bytes) -> bool:
    """True when the bytes hold a whole JPEG/PNG/WebP, not the first part of one."""
    if data.startswith(b"\xff\xd8"):
        return b"\xff\xd9" in data[-64:]
    if data.startswith(b"\x89PNG"):
        return b"IEND" in data[-16:]
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return len(data) >= 8 + int.from_bytes(data[4:8], "little")
    return False


def _content_type(data: bytes) -> str:
    if data.startswith(b"\x89PNG"):
        return "image/png"
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "image/webp"
    return "image/jpeg"

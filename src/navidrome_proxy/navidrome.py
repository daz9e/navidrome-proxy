"""Talking to the upstream Navidrome server."""

from __future__ import annotations

import asyncio
import json
import logging
import time
from dataclasses import dataclass
from typing import Any

import aiohttp
from aiohttp import web

from .subsonic.params import Params, redact
from .subsonic.response import ServerIdentity

log = logging.getLogger(__name__)

HOP_BY_HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
}


class UpstreamError(Exception):
    """Navidrome answered with ``status="failed"``."""

    def __init__(self, code: int, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class UpstreamUnavailable(Exception):
    """Navidrome could not be reached."""


@dataclass
class AuthResult:
    ok: bool
    unavailable: bool = False
    code: int = 40
    message: str = "Wrong username or password"


class Navidrome:
    def __init__(self, base_url: str, identity: ServerIdentity, auth_ttl: float = 300.0):
        self.base_url = base_url.rstrip("/")
        self.identity = identity
        self.auth_ttl = auth_ttl
        self._api: aiohttp.ClientSession | None = None
        self._raw: aiohttp.ClientSession | None = None
        self._auth_cache: dict[tuple[str, ...], tuple[float, AuthResult]] = {}
        # Latest credentials that Navidrome accepted for each user. Kept in memory
        # only: they let background work (syncing a like once a download finishes)
        # act as that user without ever writing secrets to disk.
        self.user_credentials: dict[str, Params] = {}

    async def start(self) -> None:
        connector = aiohttp.TCPConnector(limit=64, ttl_dns_cache=300)
        timeout = aiohttp.ClientTimeout(total=None, connect=5, sock_read=120)
        self._api = aiohttp.ClientSession(connector=connector, timeout=timeout)
        self._raw = aiohttp.ClientSession(
            connector=aiohttp.TCPConnector(limit=128, ttl_dns_cache=300),
            timeout=timeout,
            auto_decompress=False,
        )

    async def close(self) -> None:
        for session in (self._api, self._raw):
            if session is not None:
                await session.close()

    @property
    def api(self) -> aiohttp.ClientSession:
        assert self._api is not None, "Navidrome.start() was not awaited"
        return self._api

    # ------------------------------------------------------------ JSON calls
    async def call(self, method: str, params: Params, timeout: float = 30.0, **extra: str) -> dict[str, Any]:
        """Call ``method`` and return the ``subsonic-response`` object.

        Raises :class:`UpstreamError` for ``status="failed"`` responses.
        """
        query = params.credentials().with_(f="json", **extra)
        if "c" not in query:
            query = query.with_(c="navidrome-proxy")
        if "v" not in query:
            query = query.with_(v="1.16.1")
        url = f"{self.base_url}/rest/{method}"
        try:
            async with self.api.get(
                url, params=query.items(), timeout=aiohttp.ClientTimeout(total=timeout)
            ) as resp:
                raw = await resp.read()
        except (aiohttp.ClientError, TimeoutError) as exc:
            raise UpstreamUnavailable(f"{method}: {exc}") from exc
        try:
            root = json.loads(raw)["subsonic-response"]
        except (ValueError, KeyError) as exc:
            raise UpstreamUnavailable(f"{method}: unexpected response {raw[:200]!r}") from exc
        if root.get("status") != "ok":
            error = root.get("error") or {}
            raise UpstreamError(int(error.get("code", 0)), str(error.get("message", "failed")))
        return root

    async def call_many(
        self, method: str, params: Params, repeated: dict[str, list[str]], **extra: str
    ) -> dict[str, Any]:
        """Like :meth:`call` but with repeated parameters (``id=1&id=2``)."""
        query = params.credentials().with_(f="json", **extra)
        for key, values in repeated.items():
            query = query.replace(key, values)
        if "c" not in query:
            query = query.with_(c="navidrome-proxy")
        if "v" not in query:
            query = query.with_(v="1.16.1")
        try:
            async with self.api.get(f"{self.base_url}/rest/{method}", params=query.items()) as resp:
                root = json.loads(await resp.read())["subsonic-response"]
        except (aiohttp.ClientError, TimeoutError, ValueError, KeyError) as exc:
            raise UpstreamUnavailable(f"{method}: {exc}") from exc
        if root.get("status") != "ok":
            error = root.get("error") or {}
            raise UpstreamError(int(error.get("code", 0)), str(error.get("message", "failed")))
        return root

    # ------------------------------------------------------------------ auth
    def _auth_key(self, params: Params) -> tuple[str, ...]:
        return tuple(params.get(k) for k in ("u", "p", "t", "s", "apiKey"))

    async def authenticate(self, params: Params) -> AuthResult:
        """Validate the caller's credentials against Navidrome (cached)."""
        if not params.has_credentials:
            return AuthResult(False, code=10, message="Required parameter is missing")
        key = self._auth_key(params)
        now = time.monotonic()
        cached = self._auth_cache.get(key)
        if cached and cached[0] > now:
            if cached[1].ok:
                self.user_credentials[params.username] = params.credentials()
            return cached[1]
        try:
            root = await self.call("ping", params, timeout=10)
        except UpstreamError as exc:
            result = AuthResult(False, code=exc.code, message=exc.message)
            self._auth_cache[key] = (now + 15, result)
            return result
        except UpstreamUnavailable as exc:
            log.warning("auth check failed, Navidrome unavailable: %s", exc)
            return AuthResult(False, unavailable=True, code=0, message="Navidrome is unavailable")
        self.identity.update_from(root)
        result = AuthResult(True)
        if len(self._auth_cache) > 10_000:
            self._auth_cache.clear()
        self._auth_cache[key] = (now + self.auth_ttl, result)
        self.user_credentials[params.username] = params.credentials()
        return result

    def credentials_for(self, username: str) -> Params | None:
        return self.user_credentials.get(username)

    async def refresh_identity(self, params: Params | None = None) -> None:
        """Learn the upstream version strings (an unauthenticated ping still carries them)."""
        try:
            async with self.api.get(f"{self.base_url}/rest/ping", params={"f": "json"}) as resp:
                root = json.loads(await resp.read())["subsonic-response"]
            self.identity.update_from(root)
        except Exception as exc:  # pragma: no cover - best effort at startup
            log.warning("could not read Navidrome identity: %s", exc)

    # ----------------------------------------------------------- passthrough
    async def passthrough(
        self,
        request: web.Request,
        path: str | None = None,
        query: str | None = None,
        body: bytes | None = None,
    ) -> web.StreamResponse:
        """Relay a request to Navidrome byte-for-byte, streaming the response."""
        assert self._raw is not None
        target = f"{self.base_url}{path if path is not None else request.path}"
        qs = request.query_string if query is None else query
        if qs:
            target += "?" + qs
        headers = {k: v for k, v in request.headers.items() if k.lower() not in HOP_BY_HOP}
        headers["Host"] = request.host
        headers["X-Forwarded-Host"] = request.host
        headers["X-Forwarded-Proto"] = request.headers.get("X-Forwarded-Proto", request.scheme)
        if request.remote:
            prior = request.headers.get("X-Forwarded-For")
            headers["X-Forwarded-For"] = f"{prior}, {request.remote}" if prior else request.remote
        data = body if body is not None else (await request.read() if request.can_read_body else None)
        try:
            upstream = await self._raw.request(
                request.method, target, headers=headers, data=data, allow_redirects=False
            )
        except (aiohttp.ClientError, TimeoutError) as exc:
            log.warning("passthrough %s failed: %s", redact(target), exc)
            raise UpstreamUnavailable(str(exc)) from exc
        try:
            response = web.StreamResponse(status=upstream.status, reason=upstream.reason)
            for key, value in upstream.headers.items():
                if key.lower() not in HOP_BY_HOP:
                    response.headers.add(key, value)
            length = upstream.headers.get("Content-Length")
            if length is not None:
                response.content_length = int(length)
            await response.prepare(request)
            if request.method != "HEAD":
                async for chunk in upstream.content.iter_chunked(256 * 1024):
                    await response.write(chunk)
            await response.write_eof()
            return response
        except (ConnectionResetError, asyncio.CancelledError):
            raise
        finally:
            upstream.release()

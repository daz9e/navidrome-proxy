"""Subsonic endpoints the proxy answers itself (everything else is relayed)."""

from __future__ import annotations

from collections.abc import Awaitable, Callable

from aiohttp import web

from ..context import Call, Services

# A handler returns a response, or None to let the request pass through unchanged.
Handler = Callable[[Services, Call], Awaitable[web.StreamResponse | None]]

HANDLERS: dict[str, Handler] = {}


def endpoint(*names: str) -> Callable[[Handler], Handler]:
    def register(func: Handler) -> Handler:
        for name in names:
            HANDLERS[name] = func
        return func

    return register


def load() -> dict[str, Handler]:
    """Import handler modules (registration happens at import time)."""
    from . import browse, likes, playback, playlists, search  # noqa: F401

    return HANDLERS

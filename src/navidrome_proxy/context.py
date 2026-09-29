"""Shared services and the per-request context handed to endpoint handlers."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from aiohttp import web

from .subsonic import response as sr
from .subsonic.params import Params

if TYPE_CHECKING:
    from .catalog import Catalog
    from .config import Settings
    from .covers import Covers
    from .db import Store
    from .deezer import Deezer
    from .identity import Identity
    from .intents import Intents
    from .library import Indexer, Library
    from .navidrome import Navidrome
    from .pipeline import Pipeline
    from .scheduler import Scheduler
    from .sources import Sources


@dataclass
class Services:
    settings: Settings
    store: Store
    identity: Identity
    navidrome: Navidrome
    deezer: Deezer
    finder: Sources
    library: Library
    indexer: Indexer
    covers: Covers
    pipeline: Pipeline
    scheduler: Scheduler
    catalog: Catalog
    intents: Intents
    extra: dict[str, Any] = field(default_factory=dict)


@dataclass
class Call:
    """One Subsonic API call."""

    request: web.Request
    method: str
    params: Params
    body: bytes

    @property
    def username(self) -> str:
        return self.params.username

    @property
    def simple(self) -> dict[str, str]:
        return self.params.simple()

    def ok(self, identity: sr.ServerIdentity, payload: dict | None = None) -> web.Response:
        body, ctype = sr.ok(identity, self.simple, payload)
        return web.Response(body=body, headers={"Content-Type": ctype})

    def error(self, identity: sr.ServerIdentity, code: int, message: str) -> web.Response:
        body, ctype = sr.error(identity, self.simple, code, message)
        return web.Response(body=body, headers={"Content-Type": ctype})

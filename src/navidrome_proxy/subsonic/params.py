"""Request parameters of a Subsonic call.

Clients send parameters in the query string or, with the OpenSubsonic ``formPost``
extension, as an urlencoded POST body. Several parameters repeat (``id`` in
``star``, ``songIdToAdd`` in ``updatePlaylist``), so they are kept as a multi-map.
"""

from __future__ import annotations

import re
from collections.abc import Iterable, Iterator
from urllib.parse import parse_qsl, urlencode

CREDENTIAL_KEYS = ("u", "p", "t", "s", "apiKey")
_REDACT = re.compile(r"([?&](?:p|t|s|apiKey)=)[^&\s\"]*", re.IGNORECASE)


class Params:
    """An ordered multi-map of request parameters."""

    def __init__(self, items: Iterable[tuple[str, str]] = ()):
        self._items: list[tuple[str, str]] = [(str(k), str(v)) for k, v in items]

    @classmethod
    def parse(cls, query: str, body: str = "") -> Params:
        items = parse_qsl(query, keep_blank_values=True)
        if body:
            items += parse_qsl(body, keep_blank_values=True)
        return cls(items)

    def get(self, key: str, default: str = "") -> str:
        for k, v in self._items:
            if k == key:
                return v
        return default

    def getall(self, key: str) -> list[str]:
        return [v for k, v in self._items if k == key]

    def int(self, key: str, default: int) -> int:
        try:
            return int(self.get(key, str(default)))
        except ValueError:
            return default

    def __contains__(self, key: object) -> bool:
        return any(k == key for k, _ in self._items)

    def __iter__(self) -> Iterator[tuple[str, str]]:
        return iter(self._items)

    def items(self) -> list[tuple[str, str]]:
        return list(self._items)

    def replace(self, key: str, values: Iterable[str]) -> Params:
        """Return a copy where every ``key`` is replaced by ``values`` (possibly none)."""
        values = list(values)
        out: list[tuple[str, str]] = []
        inserted = False
        for k, v in self._items:
            if k == key:
                if not inserted:
                    out.extend((key, value) for value in values)
                    inserted = True
                continue
            out.append((k, v))
        if not inserted:
            out.extend((key, value) for value in values)
        return Params(out)

    def without(self, *keys: str) -> Params:
        return Params((k, v) for k, v in self._items if k not in keys)

    def with_(self, **values: str) -> Params:
        out = self
        for key, value in values.items():
            out = out.replace(key, [value])
        return out

    def credentials(self) -> Params:
        """Only the auth/protocol parameters, for calls the proxy makes itself."""
        keep = (*CREDENTIAL_KEYS, "v", "c")
        return Params((k, v) for k, v in self._items if k in keep)

    def encode(self) -> str:
        return urlencode(self._items)

    def simple(self) -> dict[str, str]:
        """First value of every key (what response rendering needs: ``f``, ``callback``)."""
        out: dict[str, str] = {}
        for k, v in self._items:
            out.setdefault(k, v)
        return out

    @property
    def username(self) -> str:
        return self.get("u")

    @property
    def has_credentials(self) -> bool:
        return bool(self.get("u") and (self.get("p") or (self.get("t") and self.get("s")))) or bool(
            self.get("apiKey")
        )

    def __repr__(self) -> str:
        return f"Params({redact(self.encode())})"


def redact(text: str) -> str:
    """Hide secrets in anything that is logged."""
    if not text:
        return text
    prefixed = text if text.startswith(("?", "&")) else "&" + text
    cleaned = _REDACT.sub(r"\1***", prefixed)
    return cleaned if text.startswith(("?", "&")) else cleaned[1:]

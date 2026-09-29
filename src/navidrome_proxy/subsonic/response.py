"""Subsonic response envelopes in JSON and XML.

Navidrome answers every API error with HTTP 200 and ``status="failed"``; the proxy
does the same so strict clients (Swift ``Codable`` decoders in Arpeggi) see exactly
the shapes they already handle.
"""

from __future__ import annotations

import json
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from typing import Any

XMLNS = "http://subsonic.org/restapi"

# Error codes from the Subsonic API specification.
GENERIC = 0
MISSING_PARAMETER = 10
WRONG_CREDENTIALS = 40
NOT_AUTHORIZED = 50
NOT_FOUND = 70


@dataclass
class ServerIdentity:
    """What the proxy claims to be; refreshed from the upstream ``ping``."""

    version: str = "1.16.1"
    type: str = "navidrome"
    server_version: str = "0.63.2"
    open_subsonic: bool = True

    def update_from(self, root: dict[str, Any]) -> None:
        self.version = str(root.get("version") or self.version)
        self.type = str(root.get("type") or self.type)
        self.server_version = str(root.get("serverVersion") or self.server_version)
        self.open_subsonic = bool(root.get("openSubsonic", self.open_subsonic))


def wants_xml(params: dict[str, str]) -> bool:
    return params.get("f", "xml").lower() not in ("json", "jsonp")


def envelope(identity: ServerIdentity, payload: dict[str, Any] | None = None, status: str = "ok") -> dict:
    root: dict[str, Any] = {
        "status": status,
        "version": identity.version,
        "type": identity.type,
        "serverVersion": identity.server_version,
        "openSubsonic": identity.open_subsonic,
    }
    if payload:
        root.update(payload)
    return {"subsonic-response": root}


def _xml_scalar(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def _xml_fill(element: ET.Element, payload: dict[str, Any]) -> None:
    for key, value in payload.items():
        if value is None:
            continue
        if isinstance(value, dict):
            child = ET.SubElement(element, key)
            _xml_fill(child, value)
        elif isinstance(value, list):
            for item in value:
                child = ET.SubElement(element, key)
                if isinstance(item, dict):
                    _xml_fill(child, item)
                else:
                    child.text = _xml_scalar(item)
        elif key == "value" and element.tag in ("line", "biography"):
            element.text = _xml_scalar(value)
        else:
            element.set(key, _xml_scalar(value))


def to_xml(document: dict[str, Any]) -> bytes:
    root_payload = document["subsonic-response"]
    root = ET.Element("subsonic-response", {"xmlns": XMLNS})
    _xml_fill(root, root_payload)
    return ET.tostring(root, encoding="utf-8", xml_declaration=True)


def render(params: dict[str, str], document: dict[str, Any]) -> tuple[bytes, str]:
    """Serialize ``document`` in the format the client asked for."""
    fmt = params.get("f", "xml").lower()
    if fmt == "jsonp" and params.get("callback"):
        body = json.dumps(document, ensure_ascii=False, separators=(",", ":"))
        return f"{params['callback']}({body});".encode(), "application/javascript; charset=utf-8"
    if fmt in ("json", "jsonp"):
        body = json.dumps(document, ensure_ascii=False, separators=(",", ":")).encode()
        return body, "application/json; charset=utf-8"
    return to_xml(document), "application/xml; charset=utf-8"


def ok(identity: ServerIdentity, params: dict[str, str], payload: dict[str, Any] | None = None):
    return render(params, envelope(identity, payload))


def error(identity: ServerIdentity, params: dict[str, str], code: int, message: str):
    return render(params, envelope(identity, {"error": {"code": code, "message": message}}, "failed"))

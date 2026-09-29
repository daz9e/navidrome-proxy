"""In-process fakes of the upstream services, served by real aiohttp servers."""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass, field
from typing import Any

from aiohttp import web

IDENTITY = {"version": "1.16.1", "type": "navidrome", "serverVersion": "0.63.2 (fake)", "openSubsonic": True}
USERS = {"alice": "alicepw", "bob": "bobpw"}


def ok(payload: dict | None = None) -> dict:
    return {"subsonic-response": {"status": "ok", **IDENTITY, **(payload or {})}}


def failed(code: int, message: str) -> dict:
    return {
        "subsonic-response": {"status": "failed", **IDENTITY, "error": {"code": code, "message": message}}
    }


def song(song_id: str, title: str, artist: str, album: str = "Album", album_id: str = "al1", **extra) -> dict:
    base = {
        "id": song_id,
        "parent": album_id,
        "isDir": False,
        "title": title,
        "album": album,
        "artist": artist,
        "track": 1,
        "year": 2020,
        "genre": "",
        "coverArt": f"mf-{song_id}",
        "size": 1000,
        "contentType": "audio/mp4",
        "suffix": "m4a",
        "duration": 200,
        "bitRate": 128,
        "path": f"{song_id}.m4a",
        "discNumber": 1,
        "created": "2026-01-01T00:00:00Z",
        "albumId": album_id,
        "artistId": "ar1",
        "type": "music",
        "bpm": 0,
        "comment": "",
        "sortName": title.lower(),
        "mediaType": "song",
        "musicBrainzId": "",
        "isrc": [],
        "genres": [],
        "replayGain": {},
        "channelCount": 2,
        "samplingRate": 44100,
        "bitDepth": 16,
        "moods": [],
        "artists": [{"id": "ar1", "name": artist}],
        "displayArtist": artist,
        "albumArtists": [{"id": "ar1", "name": artist}],
        "displayAlbumArtist": artist,
        "contributors": [],
        "displayComposer": "",
        "explicitStatus": "",
        "groupings": [],
        "works": [],
        "movements": [],
    }
    base.update(extra)
    return base


@dataclass
class FakeNavidrome:
    """A tiny Navidrome: real auth, a mutable song list, stars, playlists and scans."""

    songs: dict[str, dict] = field(default_factory=dict)
    stars: dict[str, set[str]] = field(default_factory=dict)
    playlists: dict[str, dict] = field(default_factory=dict)
    scrobbles: list[tuple[str, str, str]] = field(default_factory=list)
    scans: int = 0
    requests: list[tuple[str, dict]] = field(default_factory=list)
    audio: bytes = b"\x00" * 50_000
    on_scan: Any = None

    def app(self) -> web.Application:
        app = web.Application()
        app.router.add_route("*", "/rest/{method}", self.handle)
        app.router.add_route("*", "/{tail:.*}", self.other)
        return app

    def auth(self, q) -> str | None:
        user = q.get("u", "")
        pw = USERS.get(user)
        if pw is None:
            return None
        if q.get("p"):
            given = q["p"]
            if given.startswith("enc:"):
                given = bytes.fromhex(given[4:]).decode()
            return user if given == pw else None
        if q.get("t") and q.get("s"):
            return user if hashlib.md5((pw + q["s"]).encode()).hexdigest() == q["t"] else None
        return None

    async def other(self, request: web.Request) -> web.Response:
        return web.Response(text=f"navidrome ui {request.path}", headers={"X-Seen-Host": request.host})

    async def handle(self, request: web.Request) -> web.StreamResponse:
        method = request.match_info["method"].removesuffix(".view")
        q = dict(request.query)
        if request.method == "POST":
            q.update(dict(await request.post()))
        multi = request.query
        self.requests.append((method, q))
        user = self.auth(q)
        if method == "ping" and not q.get("u"):
            return web.json_response(ok())
        if user is None:
            return web.json_response(failed(40, "Wrong username or password"))
        if method == "ping":
            return web.json_response(ok())
        if method in ("search2", "search3"):
            query = q.get("query", "").strip('"').lower()
            count = int(q.get("songCount", 20))
            offset = int(q.get("songOffset", 0))
            found = [
                s
                for s in self.songs.values()
                if not query or query in f"{s['title']} {s['artist']} {s['path']}".lower()
            ]
            key = "searchResult3" if method == "search3" else "searchResult2"
            return web.json_response(ok({key: {"song": found[offset : offset + count]} if count else {}}))
        if method == "getSong":
            s = self.songs.get(q.get("id", ""))
            return web.json_response(ok({"song": s}) if s else failed(70, "Song not found"))
        if method in ("getStarred", "getStarred2"):
            key = "starred2" if method == "getStarred2" else "starred"
            songs = [
                dict(self.songs[i], starred="2026-01-01T00:00:00Z")
                for i in sorted(self.stars.get(user, set()))
                if i in self.songs
            ]
            return web.json_response(ok({key: {"song": songs} if songs else {}}))
        if method in ("star", "unstar"):
            ids = multi.getall("id", [])
            for i in ids:
                if i not in self.songs:
                    return web.json_response(failed(70, "Song not found"))
            target = self.stars.setdefault(user, set())
            for i in ids:
                (target.add if method == "star" else target.discard)(i)
            return web.json_response(ok())
        if method == "scrobble":
            for i in multi.getall("id", []):
                if i not in self.songs:
                    return web.json_response(failed(70, "not found"))
                self.scrobbles.append((user, i, q.get("submission", "true")))
            return web.json_response(ok())
        if method == "startScan":
            self.scans += 1
            if self.on_scan:
                self.on_scan(self)
            return web.json_response(ok({"scanStatus": {"scanning": True, "count": len(self.songs)}}))
        if method == "updatePlaylist":
            pl = self.playlists.setdefault(q["playlistId"], {"songs": []})
            for i in multi.getall("songIdToAdd", []):
                if i not in self.songs:
                    return web.json_response(failed(70, "not found"))
                pl["songs"].append(i)
            return web.json_response(ok())
        if method in ("stream", "download"):
            s = self.songs.get(q.get("id", ""))
            if not s:
                return web.json_response(failed(70, "not found"))
            data = self.audio
            rng = request.headers.get("Range")
            if rng:
                start, _, end = rng.removeprefix("bytes=").partition("-")
                start = int(start)
                end = int(end) if end else len(data) - 1
                return web.Response(
                    status=206,
                    body=data[start : end + 1],
                    content_type="audio/mp4",
                    headers={"Content-Range": f"bytes {start}-{end}/{len(data)}", "Accept-Ranges": "bytes"},
                )
            return web.Response(body=data, content_type="audio/mp4", headers={"Accept-Ranges": "bytes"})
        return web.json_response(ok({"echo": {"method": method, "params": json.dumps(q)}}))

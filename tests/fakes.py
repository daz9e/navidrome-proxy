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
            words = query.split()
            count = int(q.get("songCount", 20))
            offset = int(q.get("songOffset", 0))
            found = [
                s
                for s in self.songs.values()
                if all(w in f"{s['title']} {s['artist']} {s['album']}".lower() for w in words)
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
        if method == "getAlbum":
            songs = [x for x in self.songs.values() if x["albumId"] == q.get("id")]
            if not songs:
                return web.json_response(failed(70, "Album not found"))
            album = {
                "id": q["id"],
                "name": songs[0]["album"],
                "artist": songs[0]["artist"],
                "artistId": "ar1",
                "coverArt": f"al-{q['id']}",
                "songCount": len(songs),
                "duration": 0,
                "song": songs,
            }
            return web.json_response(ok({"album": album}))
        if method == "getTopSongs":
            return web.json_response(ok({"topSongs": {}}))
        if method == "createPlaylist":
            pid = q.get("playlistId") or f"pl{len(self.playlists) + 1}"
            self.playlists[pid] = {"songs": list(multi.getall("songId", []))}
            return web.json_response(ok({"playlist": {"id": pid, "name": q.get("name", ""), "entry": []}}))
        if method == "getPlaylist":
            pl = self.playlists.get(q.get("id", ""))
            if pl is None:
                return web.json_response(failed(70, "not found"))
            entries = [self.songs[i] for i in pl["songs"] if i in self.songs]
            return web.json_response(
                ok(
                    {
                        "playlist": {
                            "id": q["id"],
                            "name": "p",
                            "songCount": len(entries),
                            "duration": 0,
                            "entry": entries,
                        }
                    }
                )
            )
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


def dz_track(
    tid: int,
    title: str,
    artist: str,
    album_id: int = 100,
    album: str = "Album",
    duration: int = 200,
    artist_id: int = 1,
    contributors: list[str] | None = None,
    position: int = 1,
    **extra,
) -> dict:
    t = {
        "id": tid,
        "type": "track",
        "readable": True,
        "title": title,
        "title_version": "",
        "duration": duration,
        "rank": 1000,
        "explicit_lyrics": False,
        "md5_image": f"md5{album_id}",
        "artist": {"id": artist_id, "name": artist, "picture_xl": f"https://img/{artist_id}.jpg"},
        "album": {
            "id": album_id,
            "title": album,
            "md5_image": f"md5{album_id}",
            "release_date": "2020-05-01",
        },
        "track_position": position,
        "disk_number": 1,
        "isrc": f"ISRC{tid}",
    }
    if contributors is not None:
        t["contributors"] = [{"id": i, "name": n, "role": "Main"} for i, n in enumerate(contributors)]
    t.update(extra)
    return t


@dataclass
class FakeDeezer:
    tracks: dict[int, dict] = field(default_factory=dict)
    albums: dict[int, dict] = field(default_factory=dict)
    artists: dict[int, dict] = field(default_factory=dict)
    hits: list[str] = field(default_factory=list)
    quota_failures: int = 0

    def add_album(
        self,
        album_id: int,
        title: str,
        artist: str,
        artist_id: int,
        tracks: list[dict],
        release_date: str = "2020-05-01",
    ) -> None:
        self.albums[album_id] = {
            "id": album_id,
            "title": title,
            "md5_image": f"md5{album_id}",
            "release_date": release_date,
            "nb_tracks": len(tracks),
            "record_type": "album",
            "explicit_lyrics": False,
            "genres": {"data": [{"id": 1, "name": "Rap"}]},
            "artist": {"id": artist_id, "name": artist},
            "tracks": {"data": tracks},
        }
        for t in tracks:
            self.tracks[t["id"]] = t
        self.artists.setdefault(
            artist_id,
            {"id": artist_id, "name": artist, "nb_album": 1, "picture_xl": f"https://img/{artist_id}.jpg"},
        )

    def app(self) -> web.Application:
        app = web.Application()
        app.router.add_get("/{path:.*}", self.handle)
        return app

    async def handle(self, request: web.Request) -> web.Response:
        path = request.match_info["path"]
        self.hits.append(request.path_qs)
        if self.quota_failures > 0:
            self.quota_failures -= 1
            return web.json_response(
                {"error": {"type": "Exception", "message": "Quota limit exceeded", "code": 4}}
            )
        parts = path.split("/")
        q = request.query.get("q", "").lower()
        if parts[0] == "search":
            kind = parts[1] if len(parts) > 1 else "track"
            if kind == "track":
                data = [
                    dict({k: v for k, v in t.items() if k != "contributors"})
                    for t in self.tracks.values()
                    if all(w in f"{t['title']} {t['artist']['name']}".lower() for w in q.split())
                ]
            elif kind == "album":
                data = [
                    {k: v for k, v in a.items() if k != "tracks"}
                    for a in self.albums.values()
                    if all(w in f"{a['title']} {a['artist']['name']}".lower() for w in q.split())
                ]
            else:
                data = [a for a in self.artists.values() if q and q in a["name"].lower()]
            return web.json_response({"data": data, "total": len(data)})
        if parts[0] == "track" and int(parts[1]) in self.tracks:
            t = dict(self.tracks[int(parts[1])])
            t.setdefault("contributors", [{"id": 0, "name": t["artist"]["name"], "role": "Main"}])
            return web.json_response(t)
        if parts[0] == "album" and int(parts[1]) in self.albums:
            album = self.albums[int(parts[1])]
            if len(parts) > 2 and parts[2] == "tracks":
                return web.json_response(
                    {"data": album["tracks"]["data"], "total": len(album["tracks"]["data"])}
                )
            return web.json_response(album)
        if parts[0] == "artist" and int(parts[1]) in self.artists:
            aid = int(parts[1])
            if len(parts) > 2 and parts[2] == "albums":
                data = [
                    {k: v for k, v in a.items() if k != "tracks"}
                    for a in self.albums.values()
                    if a["artist"]["id"] == aid
                ]
                return web.json_response({"data": data, "total": len(data)})
            if len(parts) > 2 and parts[2] == "top":
                data = [t for t in self.tracks.values() if t["artist"]["id"] == aid]
                return web.json_response({"data": data[: int(request.query.get("limit", 5))]})
            if len(parts) > 2 and parts[2] == "related":
                return web.json_response({"data": [a for a in self.artists.values() if a["id"] != aid]})
            return web.json_response(self.artists[aid])
        return web.json_response({"error": {"type": "DataException", "message": "no data", "code": 800}})

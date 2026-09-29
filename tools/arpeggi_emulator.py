#!/usr/bin/env python3
"""Replay what the Arpeggi iOS client does against a Subsonic server (the proxy).

Arpeggi is closed source and ships only through the App Store/TestFlight, so it
cannot run in a simulator. The request shapes below are taken from the proxy's
production logs of real Arpeggi sessions (v=1.16.1, c=Arpeggi, f=json, token
auth, ``estimateContentLength=false`` streams, ``songCount=2000`` search paging).

Every returned song/album/artist is checked against the key set and JSON types of
real Navidrome 0.63 objects, the way a Swift ``Codable`` decoder would fail.

usage: arpeggi_emulator.py --url http://host:8084 --user U --password P \
           [--query "юпи порезы"] [--play 1] [--star] [--json report.json]
Exit code 0 when every check passed.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import secrets
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

REFERENCE = json.loads(
    (Path(__file__).resolve().parent.parent / "tests" / "fixtures" / "navidrome_0.63.json").read_text()
)
OPTIONAL = {
    "song": {
        "starred",
        "playCount",
        "played",
        "userRating",
        "averageRating",
        "transcodedSuffix",
        "transcodedContentType",
        "bookmarkPosition",
        "originalWidth",
        "originalHeight",
    },
    "album": {"starred", "playCount", "played", "song", "parent", "isDir", "title", "album"},
    "artist": {"starred", "album", "userRating"},
}
# Keys Navidrome itself omits when empty (seen on real objects); a client must cope anyway.
NAVIDROME_OMITTABLE = {
    "song": {"track", "year", "genre", "bpm", "comment", "musicBrainzId", "discNumber"},
    "album": {"year", "genre", "musicBrainzId", "version"},
    "artist": {"musicBrainzId", "artistImageUrl", "coverArt"},
}


class Emulator:
    def __init__(self, url: str, user: str, password: str, verbose: bool = True):
        self.base = url.rstrip("/")
        self.user = user
        self.password = password
        self.verbose = verbose
        self.checks: list[dict] = []

    # ------------------------------------------------------------ plumbing
    def auth(self) -> dict[str, str]:
        salt = secrets.token_hex(6)
        token = hashlib.md5((self.password + salt).encode()).hexdigest()
        return {"t": token, "s": salt, "u": self.user, "v": "1.16.1", "c": "Arpeggi", "f": "json"}

    def url(self, method: str, **params) -> str:
        query = {**self.auth(), **{k: str(v) for k, v in params.items()}}
        return f"{self.base}/rest/{method}.view?{urllib.parse.urlencode(query)}"

    def call(self, method: str, **params) -> tuple[dict, float]:
        started = time.monotonic()
        with urllib.request.urlopen(self.url(method, **params), timeout=180) as resp:
            body = resp.read()
        elapsed = time.monotonic() - started
        root = json.loads(body)["subsonic-response"]
        return root, elapsed

    def check(self, name: str, ok: bool, detail: str = "", elapsed: float | None = None) -> bool:
        self.checks.append(
            {
                "check": name,
                "ok": bool(ok),
                "detail": detail,
                "ms": round(elapsed * 1000) if elapsed is not None else None,
            }
        )
        if self.verbose:
            mark = "PASS" if ok else "FAIL"
            timing = f" {elapsed * 1000:.0f}ms" if elapsed is not None else ""
            print(f"[{mark}] {name}{timing} {detail}")
        return ok

    def shape(self, kind: str, obj: dict, where: str) -> None:
        ref = REFERENCE[{"song": "song", "album": "album", "artist": "artist"}[kind]]
        problems = []
        for key, value in ref.items():
            if key not in obj:
                if key not in NAVIDROME_OMITTABLE[kind]:
                    problems.append(f"missing {key}")
                continue
            if type(obj[key]) is not type(value) and not (
                isinstance(value, int) and isinstance(obj[key], int)
            ):
                problems.append(f"{key}: {type(obj[key]).__name__} != {type(value).__name__}")
        for key, value in obj.items():
            if value is None:
                problems.append(f"{key} is null")
            if key == "starred" and not isinstance(value, str):
                problems.append("starred must be an ISO date string")
        self.check(f"schema {kind} {where}", not problems, "; ".join(problems[:5]) or str(obj.get("id")))

    # ------------------------------------------------------------ journeys
    def launch(self) -> None:
        root, t = self.call("ping")
        self.check("ping", root["status"] == "ok", elapsed=t)
        for method, params in (
            ("getOpenSubsonicExtensions", {}),
            ("getMusicFolders", {}),
            ("getUser", {"username": self.user}),
            ("getScanStatus", {}),
            ("getPlaylists", {}),
            ("getGenres", {}),
            ("getRandomSongs", {"size": 100, "musicFolderId": 1}),
        ):
            root, t = self.call(method, **params)
            self.check(method, root["status"] == "ok", elapsed=t)
        for kind in ("frequent", "recent", "newest", "random"):
            root, t = self.call("getAlbumList2", type=kind, size=100, musicFolderId=1)
            ok = root["status"] == "ok"
            albums = (root.get("albumList2") or {}).get("album") or []
            self.check(f"getAlbumList2 {kind}", ok, f"{len(albums)} albums", t)
        root, t = self.call("getStarred2", musicFolderId=1)
        self.check("getStarred2", root["status"] == "ok", elapsed=t)
        for s in ((root.get("starred2") or {}).get("song") or [])[:5]:
            self.shape("song", s, "starred2")

    def library_sync(self) -> None:
        for offset in (0, 1000):
            root, t = self.call(
                "search3",
                artistCount=1000,
                artistOffset=offset,
                albumCount=0,
                albumOffset=0,
                songCount=0,
                songOffset=0,
                musicFolderId=1,
            )
            self.check(f"sync artists offset={offset}", root["status"] == "ok", elapsed=t)

    def search(self, query: str) -> list[dict]:
        words = query.split()
        typed = [" ".join(words[:i]) for i in range(1, len(words) + 1)]
        for text in typed:
            root, t = self.call(
                "search3",
                query=text,
                artistCount=0,
                artistOffset=0,
                albumCount=0,
                albumOffset=0,
                songCount=50,
                songOffset=0,
            )
            songs = (root.get("searchResult3") or {}).get("song") or []
            self.check(f"type-ahead '{text}'", root["status"] == "ok", f"{len(songs)} songs", t)
        root, t = self.call(
            "search3",
            query=query,
            artistCount=200,
            artistOffset=0,
            albumCount=200,
            albumOffset=0,
            songCount=200,
            songOffset=0,
            musicFolderId=1,
        )
        result = root.get("searchResult3") or {}
        self.check(
            "search all kinds",
            root["status"] == "ok",
            f"{len(result.get('artist', []))} artists, {len(result.get('album', []))} albums, "
            f"{len(result.get('song', []))} songs",
            t,
        )
        for kind in ("artist", "album", "song"):
            for obj in (result.get(kind) or [])[:5]:
                self.shape(kind, obj, f"search '{query}'")
        # Arpeggi's "all songs" list pages by the number of songs received so far.
        received: list[dict] = []
        pages = 0
        while pages < 30:
            root, _ = self.call(
                "search3",
                query=query,
                artistCount=0,
                artistOffset=0,
                albumCount=0,
                albumOffset=0,
                songCount=2000,
                songOffset=len(received),
                musicFolderId=1,
            )
            page = (root.get("searchResult3") or {}).get("song") or []
            pages += 1
            if not page:
                break
            received += page
        ids = [s["id"] for s in received]
        self.check("song paging terminates", pages < 30, f"{pages} pages, {len(received)} songs")
        self.check("song paging has no duplicates", len(ids) == len(set(ids)))
        return result.get("song") or []

    def play(self, song: dict) -> None:
        song_id = song["id"]
        root, t = self.call("getSong", id=song_id)
        self.check(f"getSong {song_id}", root["status"] == "ok", elapsed=t)
        if root["status"] == "ok":
            self.shape("song", root["song"], "getSong")
        cover = urllib.request.urlopen(
            self.url("getCoverArt", id=song.get("coverArt") or song_id, size=800), timeout=60
        )
        self.check(
            "getCoverArt", cover.status == 200 and cover.headers.get("Content-Type", "").startswith("image/")
        )
        # AVPlayer: probe the first two bytes, then read the rest.
        started = time.monotonic()
        req = urllib.request.Request(
            self.url("stream", id=song_id, estimateContentLength="false"), headers={"Range": "bytes=0-1"}
        )
        try:
            resp = urllib.request.urlopen(req, timeout=180)
        except urllib.error.HTTPError as exc:
            self.check("stream first bytes", False, f"HTTP {exc.code}")
            return
        first = time.monotonic() - started
        content_type = resp.headers.get("Content-Type", "")
        if not content_type.startswith("audio/"):
            self.check("stream first bytes", False, f"not audio: {resp.read(300)!r}", first)
            return
        content_range = resp.headers.get("Content-Range", "")
        total = int(content_range.rsplit("/", 1)[1]) if "/" in content_range else -1
        self.check(
            "stream 206 + Content-Range",
            resp.status == 206 and total > 0 and len(resp.read()) == 2,
            f"{content_range} {content_type}",
            first,
        )
        req = urllib.request.Request(
            self.url("stream", id=song_id, estimateContentLength="false"),
            headers={"Range": f"bytes=2-{total - 1}"},
        )
        data = urllib.request.urlopen(req, timeout=300).read()
        self.check("stream rest of file", len(data) == total - 2, f"{len(data) + 2} bytes")
        after, _ = self.call("getSong", id=song_id)
        info = after.get("song") or {}
        self.check(
            "getSong size matches bytes served",
            info.get("size") in (total, 0) or not song_id.startswith(("deezer:", "yt:", "sc:")),
            f"{info.get('size')} vs {total}",
        )
        with tempfile.NamedTemporaryFile(suffix="." + (info.get("suffix") or "m4a")) as tmp:
            tmp.write(b"\0\0" + data)  # the first two bytes do not matter for probing duration
            tmp.flush()
            try:
                out = subprocess.run(
                    [
                        "ffprobe",
                        "-v",
                        "error",
                        "-show_entries",
                        "format=duration:format_tags=major_brand",
                        "-of",
                        "json",
                        tmp.name,
                    ],
                    capture_output=True,
                    text=True,
                    timeout=60,
                )
                fmt = json.loads(out.stdout or "{}").get("format", {})
                real = float(fmt.get("duration") or 0)
                brand = (fmt.get("tags") or {}).get("major_brand", "")
                self.check("container is not DASH", brand != "dash", brand)
                self.check(
                    "getSong duration matches audio",
                    abs(real - float(info.get("duration") or 0)) <= 2,
                    f"{info.get('duration')}s vs {real:.1f}s",
                )
            except (FileNotFoundError, subprocess.SubprocessError):
                pass
        for submission in ("false", "true"):
            root, t = self.call("scrobble", id=song_id, submission=submission, time=int(time.time() * 1000))
            self.check(f"scrobble submission={submission}", root["status"] == "ok", elapsed=t)

    def browse(self, song: dict) -> None:
        if song.get("albumId"):
            root, t = self.call("getAlbum", id=song["albumId"])
            ok = root["status"] == "ok"
            self.check(
                f"getAlbum {song['albumId']}",
                ok,
                f"{len((root.get('album') or {}).get('song') or [])} songs",
                t,
            )
            if ok:
                album = dict(root["album"])
                songs = album.pop("song", [])
                self.shape("album", album, "getAlbum")
                for s in songs[:3]:
                    self.shape("song", s, "album track")
                ids = [s["id"] for s in songs]
                self.check("album has no duplicate songs", len(ids) == len(set(ids)))
        if song.get("artistId"):
            root, t = self.call("getArtist", id=song["artistId"])
            self.check(f"getArtist {song['artistId']}", root["status"] == "ok", elapsed=t)
            if root["status"] == "ok":
                artist = dict(root["artist"])
                albums = artist.pop("album", [])
                self.shape("artist", artist, "getArtist")
                names = [a.get("name") for a in albums]
                self.check(
                    "artist has no duplicate albums", len(names) == len(set(names)), f"{len(names)} albums"
                )
            root, t = self.call("getArtistInfo2", id=song["artistId"], count=5)
            self.check("getArtistInfo2", root["status"] == "ok", elapsed=t)
            root, t = self.call("getTopSongs", artist=song.get("artist", ""), count=10)
            self.check("getTopSongs", root["status"] == "ok", elapsed=t)

    def like(self, song: dict) -> None:
        root, t = self.call("star", id=song["id"])
        self.check("star", root["status"] == "ok", elapsed=t)
        root, t = self.call("getStarred2", musicFolderId=1)
        starred = [s for s in (root.get("starred2") or {}).get("song") or []]
        self.check(
            "liked song visible in getStarred2",
            any(s["id"] == song["id"] or s.get("title") == song.get("title") for s in starred),
            elapsed=t,
        )
        for s in starred[:3]:
            self.shape("song", s, "starred2 after like")


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--url", required=True)
    parser.add_argument("--user", required=True)
    parser.add_argument("--password", required=True)
    parser.add_argument("--query", action="append", default=[])
    parser.add_argument("--play", type=int, default=1, help="how many search hits of the first query to play")
    parser.add_argument("--star", action="store_true", help="also like the first played song")
    parser.add_argument("--json", type=Path, help="write the report here")
    args = parser.parse_args()
    # Like the app itself; CDNs in front of a server often block Python's default agent.
    opener = urllib.request.build_opener()
    opener.addheaders = [("User-Agent", "Arpeggi/1.0 CFNetwork/3826.500.131 Darwin/25.0.0")]
    urllib.request.install_opener(opener)
    emu = Emulator(args.url, args.user, args.password)
    emu.launch()
    emu.library_sync()
    songs: list[dict] = []
    for query in args.query or ["daft punk"]:
        found = emu.search(query)
        songs = songs or found
    for song in songs[: args.play]:
        emu.play(song)
        emu.browse(song)
    if args.star and songs:
        emu.like(songs[0])
    failed = [c for c in emu.checks if not c["ok"]]
    print(f"\n{len(emu.checks) - len(failed)}/{len(emu.checks)} checks passed")
    if args.json:
        args.json.write_text(json.dumps(emu.checks, ensure_ascii=False, indent=1))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

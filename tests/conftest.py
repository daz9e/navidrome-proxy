import hashlib
import shutil
from dataclasses import dataclass
from pathlib import Path

import mutagen
import pytest
from aiohttp.test_utils import TestClient, TestServer

from navidrome_proxy import sources
from navidrome_proxy.app import SERVICES, create_app
from navidrome_proxy.config import Settings
from navidrome_proxy.context import Services

from .audio_fixtures import make
from .fakes import FakeDeezer, FakeNavidrome, dz_track, song

ALICE = "u=alice&p=alicepw&v=1.16.1&c=Arpeggi&f=json"
BOB = "u=bob&p=bobpw&v=1.16.1&c=Arpeggi&f=json"


def index_music(music: Path):
    """A Navidrome scan: every audio file becomes a song (id from path, real size)."""

    def scan(fake: FakeNavidrome) -> None:
        for path in music.rglob("*"):
            if path.suffix not in (".m4a", ".mp3") or ".navidrome-proxy-staging" in path.parts:
                continue
            tags = mutagen.File(path, easy=True) or {}
            title = (tags.get("title") or [path.stem])[0]
            artist = (tags.get("artist") or ["?"])[0]
            album = (tags.get("album") or ["[Unknown Album]"])[0]
            song_id = "L" + hashlib.md5(str(path.relative_to(music)).encode()).hexdigest()[:10]
            fake.songs[song_id] = song(
                song_id,
                title,
                artist,
                album=album,
                album_id="AL-" + album,
                size=path.stat().st_size,
                path=f"{artist}/{album}/{title}{path.suffix}",
                duration=int(mutagen.File(path).info.length),
            )

    return scan


class FakeFinder:
    def __init__(self):
        self.results: dict[str, list[sources.Candidate]] = {}
        self.ytmusic_rows: list[dict] = []
        self.flat: list[dict] = []

    async def find(self, wanted, exclude=None):
        return list(self.results.get(wanted.title, []))

    async def ytmusic(self, query, filter_="songs", limit=10):
        return [
            r
            for r in self.ytmusic_rows
            if all(
                w in r["title"].lower() or w in " ".join(r["artists"]).lower() for w in query.lower().split()
            )
        ]

    def _flat_search(self, query):
        return self.flat


class FakeDownloader:
    def __init__(self):
        self.lengths: dict[str, int] = {}
        self.calls: list[str] = []

    async def __call__(self, cand, dest, timeout=300, cancel=None):
        self.calls.append(cand.ref)
        return make(dest / "source.m4a", self.lengths.get(cand.ref, 30), dash=True)


@dataclass
class World:
    client: TestClient
    services: Services
    navidrome: FakeNavidrome
    deezer: FakeDeezer
    finder: FakeFinder
    downloader: FakeDownloader
    music: Path

    async def get(self, method: str, query: str = "", auth: str = ALICE, **kw):
        return await self.client.get(f"/rest/{method}.view?{auth}&{query}", **kw)

    async def json(self, method: str, query: str = "", auth: str = ALICE):
        resp = await self.get(method, query, auth)
        assert resp.status == 200, await resp.text()
        return (await resp.json())["subsonic-response"]


@pytest.fixture
async def world(tmp_path):
    if shutil.which("ffmpeg") is None:
        pytest.skip("ffmpeg missing")
    nav = FakeNavidrome()
    music = tmp_path / "music"
    music.mkdir()
    nav.on_scan = index_music(music)
    nav_server = TestServer(nav.app())
    await nav_server.start_server()
    dz = FakeDeezer()
    dz.add_album(
        100,
        "RAW",
        "TVETH",
        1,
        [
            dz_track(
                1001,
                "Hella Quality",
                "TVETH",
                100,
                "RAW",
                duration=30,
                contributors=["TVETH", "Bato"],
                position=1,
            ),
            dz_track(1002, "Second Song", "TVETH", 100, "RAW", duration=30, position=2),
            dz_track(1003, "Third Song", "TVETH", 100, "RAW", duration=30, position=3),
        ],
    )
    dz_server = TestServer(dz.app())
    await dz_server.start_server()
    settings = Settings.from_env(
        {
            "NAVIDROME_URL": str(nav_server.make_url("")).rstrip("/"),
            "MUSIC_DIR": str(music),
            "DATA_DIR": str(tmp_path / "data"),
            "LYRICS": "false",
            "SCAN_DEBOUNCE_SECONDS": "0.05",
            "STREAM_WAIT_SECONDS": "20",
            "SEARCH_BUDGET_SECONDS": "3",
            "DEEZER_REQUESTS_PER_SECOND": "200",
        }
    )
    finder, downloader = FakeFinder(), FakeDownloader()
    app = create_app(settings, finder=finder, downloader=downloader)
    app[SERVICES].deezer.base_url = str(dz_server.make_url("")).rstrip("/")
    client = TestClient(TestServer(app))
    await client.start_server()
    yield World(client, app[SERVICES], nav, dz, finder, downloader, music)
    await client.close()
    await dz_server.close()
    await nav_server.close()


def candidate(ref: str, title: str = "x", artist: str = "TVETH") -> sources.Candidate:
    return sources.Candidate(
        "youtube",
        f"https://music.youtube.com/watch?v={ref}",
        ref,
        title,
        f"{artist} - Topic",
        30,
        official=True,
    )

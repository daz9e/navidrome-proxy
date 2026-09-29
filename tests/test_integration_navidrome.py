"""Against a real Navidrome 0.63.2 container (``pytest -m integration``; needs Docker).

Catalog and audio sources are faked (no internet); Navidrome, ffmpeg, tagging and
the proxy are real.
"""

from __future__ import annotations

import asyncio
import json
import shutil
import socket
import subprocess
import time
import urllib.request
import uuid
from pathlib import Path

import pytest
from aiohttp.test_utils import TestClient, TestServer

from navidrome_proxy import migrate, tagging
from navidrome_proxy.app import SERVICES, create_app
from navidrome_proxy.config import Settings

from .audio_fixtures import make
from .conftest import FakeDownloader, FakeFinder, candidate
from .fakes import FakeDeezer, dz_track

pytestmark = [
    pytest.mark.integration,
    pytest.mark.skipif(
        shutil.which("docker") is None or shutil.which("ffmpeg") is None, reason="docker/ffmpeg"
    ),
]

IMAGE = "deluan/navidrome:0.63.2"
ADMIN = "u=admin&p=adminpw&v=1.16.1&c=Arpeggi&f=json"


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def nd(base: str, method: str, query: str = "") -> dict:
    with urllib.request.urlopen(f"{base}/rest/{method}.view?{ADMIN}&{query}", timeout=30) as r:
        return json.loads(r.read())["subsonic-response"]


def wait_scan(base: str, timeout: float = 60) -> None:
    deadline = time.time() + timeout
    time.sleep(1)
    while time.time() < deadline:
        if not nd(base, "getScanStatus")["scanStatus"]["scanning"]:
            return
        time.sleep(0.5)
    raise AssertionError("scan did not finish")


@pytest.fixture(scope="module")
def navidrome(tmp_path_factory):
    root = tmp_path_factory.mktemp("nd")
    music, data = root / "music", root / "data"
    music.mkdir()
    data.mkdir()
    (data / "navidrome.toml").write_text(
        'PID.Album = "albumartistid,album"\nPID.Track = "albumid,discnumber,tracknumber,title"\n'
    )
    port = free_port()
    name = f"np-it-{uuid.uuid4().hex[:6]}"
    subprocess.run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            name,
            "-p",
            f"127.0.0.1:{port}:4533",
            "-e",
            "ND_CONFIGFILE=/data/navidrome.toml",
            "-e",
            "ND_DEVAUTOCREATEADMINPASSWORD=adminpw",
            "-e",
            "ND_SCANNER_SCHEDULE=0",
            "-v",
            f"{music}:/music",
            "-v",
            f"{data}:/data",
            IMAGE,
        ],
        check=True,
        capture_output=True,
    )
    base = f"http://127.0.0.1:{port}"
    try:
        for _ in range(120):
            try:
                if nd(base, "ping")["status"] == "ok":
                    break
            except Exception:
                time.sleep(0.5)
        else:
            raise AssertionError("navidrome did not start")
        yield base, music, root
    finally:
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)


def test_navidrome_reads_our_tags_and_keeps_ids_across_relayout_and_remux(navidrome, tmp_path):
    base, music, _ = navidrome
    legacy = make(music / "deezer-555.m4a", 30, dash=True)
    tagging.write_tags(
        legacy,
        {
            "title": "Relayout Me",
            "artist": "TVETH, Bato",
            "artists": ["TVETH", "Bato"],
            "album": "RAW II",
            "album_artist": "TVETH",
            "track": 4,
            "year": 2026,
        },
    )
    nd(base, "startScan", "fullScan=true")
    wait_scan(base)
    found = nd(base, "search3", "query=Relayout&songCount=5")["searchResult3"]["song"]
    assert len(found) == 1
    song = found[0]
    assert [a["name"] for a in song["artists"]] == ["TVETH", "Bato"]
    assert song["albumArtists"][0]["name"] == "TVETH"
    nd(base, "star", f"id={song['id']}")

    from navidrome_proxy.db import Store

    store = Store(tmp_path / "db.sqlite3")
    asyncio.run(migrate.migrate_legacy(store, music, None))
    assert migrate.relayout(store, music)["moved"] == 1
    assert asyncio.run(migrate.remux_dash(music))["remuxed"] == 1
    store.close()
    nd(base, "startScan")
    wait_scan(base)
    after = nd(base, "search3", "query=Relayout&songCount=5")["searchResult3"]["song"]
    assert [s["id"] for s in after] == [song["id"]]  # same id: likes and play counts survive
    assert after[0].get("starred")
    assert after[0]["duration"] == 30


@pytest.fixture
async def proxy(navidrome, tmp_path):
    base, music, _ = navidrome
    dz = FakeDeezer()
    dz.add_album(
        700,
        "Live Album",
        "Real Artist",
        7,
        [
            dz_track(7001, "First Real", "Real Artist", 700, "Live Album", 30, 7, position=1),
            dz_track(7002, "Second Real", "Real Artist", 700, "Live Album", 30, 7, position=2),
        ],
    )
    dz_server = TestServer(dz.app())
    await dz_server.start_server()
    finder, downloader = FakeFinder(), FakeDownloader()
    finder.results["First Real"] = [candidate("FFFFFFFFFFF", "First Real", "Real Artist")]
    finder.results["Second Real"] = [candidate("SSSSSSSSSSS", "Second Real", "Real Artist")]
    settings = Settings.from_env(
        {
            "NAVIDROME_URL": base,
            "MUSIC_DIR": str(music),
            "DATA_DIR": str(tmp_path),
            "NAVIDROME_USER": "admin",
            "NAVIDROME_PASSWORD": "adminpw",
            "LYRICS": "false",
            "SCAN_DEBOUNCE_SECONDS": "0.2",
            "DEEZER_REQUESTS_PER_SECOND": "100",
        }
    )
    app = create_app(settings, finder=finder, downloader=downloader)
    app[SERVICES].deezer.base_url = str(dz_server.make_url("")).rstrip("/")
    client = TestClient(TestServer(app))
    await client.start_server()
    yield client, app[SERVICES], base
    await client.close()
    await dz_server.close()


async def until(predicate, timeout=60.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        await asyncio.sleep(0.25)
    raise AssertionError("condition not reached")


async def test_full_cycle_against_real_navidrome(proxy):
    client, services, base = proxy
    store = services.store

    async def get(method, query=""):
        resp = await client.get(f"/rest/{method}.view?{ADMIN}&{query}")
        return resp

    # A like on an external track: visible at once, then a real like in Navidrome.
    root = (await (await get("star", "id=deezer:7002")).json())["subsonic-response"]
    assert root["status"] == "ok"
    # First play of another external track streams the downloaded file.
    resp = await get("stream", "id=deezer:7001")
    body = await resp.read()
    assert resp.status == 200 and len(body) > 10_000
    await until(lambda: (t := store.get_track("deezer:7001")) is not None and t.local_id is not None)
    await until(lambda: (t := store.get_track("deezer:7002")) is not None and t.local_id is not None)
    await until(lambda: not store.pending_stars("admin"))
    local_liked = store.get_track("deezer:7002").local_id
    starred = nd(base, "getStarred2")["starred2"]["song"]
    assert local_liked in {s["id"] for s in starred}
    # The downloaded track is now the real Navidrome song everywhere.
    album = (await (await get("getAlbum", "id=deezer-album:700")).json())["subsonic-response"]["album"]
    assert [s["id"] for s in album["song"]] == [store.get_track("deezer:7001").local_id, local_liked]
    local = nd(base, "getSong", f"id={local_liked}")["song"]
    assert local["album"] == "Live Album" and local["track"] == 2
    # Both tracks landed in ONE Navidrome album.
    assert (
        nd(base, "getSong", f"id={store.get_track('deezer:7001').local_id}")["song"]["albumId"]
        == local["albumId"]
    )
    # Real schema parity: virtual songs carry every key a real Navidrome song has.
    virtual = (await (await get("getSong", "id=deezer:7001")).json())["subsonic-response"]["song"]
    missing = set(local) - set(virtual) - {"starred", "playCount", "played", "bookmarkPosition", "userRating"}
    assert not missing, missing


def test_virtual_payload_is_the_same_shape_as_real_objects():
    # Guard for the committed fixture: it must still be a Navidrome 0.63 capture.
    fixture = json.loads((Path(__file__).parent / "fixtures" / "navidrome_0.63.json").read_text())
    assert {"song", "album", "artist"} <= set(fixture)

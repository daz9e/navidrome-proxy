import logging
import shutil
import subprocess
from pathlib import Path

import pytest
from aiohttp import web
from aiohttp.test_utils import TestServer

from navidrome_proxy import matching, slskd
from navidrome_proxy.db import Store
from navidrome_proxy.library import Library

from .audio_fixtures import make_music

needs_tools = pytest.mark.skipif(not (shutil.which("ffmpeg") and shutil.which("fpcalc")), reason="tools")

WANTED = matching.Wanted("Black Water", ["FORTUNA 812"], duration=40)


def response(user, files, free=True):
    return {"username": user, "hasFreeUploadSlot": free, "queueLength": 0, "files": files}


def rfile(name, size=1000, bitrate=320, length=40):
    return {"filename": name, "size": size, "bitRate": bitrate, "length": length}


def test_select_filters_versions_bitrate_and_counts_peers():
    responses = [
        response("a", [rfile("music\\FORTUNA 812\\Project n(o)va\\08 Black Water.mp3", 5000)]),
        response("b", [rfile("x\\FORTUNA 812\\Project n(o)va\\08 Black Water.mp3", 5000)]),
        response("c", [rfile("x\\FORTUNA 812\\08 Black Water (минус).mp3", 5000)]),
        response("d", [rfile("x\\FORTUNA 812\\08 Black Water.mp3", 3000, bitrate=128)]),
        response("e", [rfile("x\\FORTUNA 812\\08 Black Water.flac", 9000, bitrate=1000)]),
        response("f", [rfile("x\\FORTUNA 812\\08 Black Water.mp3", 4000, length=90)]),
    ]
    picked = slskd.select(responses, WANTED, min_peers=2)
    assert {(r.username, r.peers) for r in picked} == {("a", 2), ("b", 2)}
    assert slskd.select(responses, WANTED, min_peers=3) == []


class FakeSlskd:
    def __init__(self, downloads: Path, files: dict[str, Path]):
        self.downloads = downloads
        self.files = files  # remote filename -> local source file
        self.transfers: dict[str, list[dict]] = {}

    def app(self):
        app = web.Application()
        app.router.add_post("/api/v0/searches", self.create)
        app.router.add_get("/api/v0/searches/{id}", self.state)
        app.router.add_get("/api/v0/searches/{id}/responses", self.responses)
        app.router.add_delete("/api/v0/searches/{id}", self.empty)
        app.router.add_post("/api/v0/transfers/downloads/{user}", self.enqueue)
        app.router.add_get("/api/v0/transfers/downloads/{user}", self.status)
        return app

    async def state(self, request):
        return web.json_response({"isComplete": True})

    async def empty(self, request):
        return web.Response(status=204)

    async def create(self, request):
        return web.json_response({"id": "s1"})

    async def responses(self, request):
        return web.json_response(
            [
                response(f"user{i}", [rfile(name, path.stat().st_size, 320, 40)])
                for i, (name, path) in enumerate(self.files.items())
            ]
        )

    async def enqueue(self, request):
        user = request.match_info["user"]
        for item in await request.json():
            src = self.files[item["filename"]]
            folder = item["filename"].split("\\")[-2]
            dest = self.downloads / folder / item["filename"].split("\\")[-1]
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy(src, dest)
            self.transfers.setdefault(user, []).append(
                {"filename": item["filename"], "state": "Completed, Succeeded"}
            )
        return web.Response(status=201)

    async def status(self, request):
        files = self.transfers.get(request.match_info["user"], [])
        return web.json_response({"directories": [{"files": files}]})


@needs_tools
async def test_upgrade_rejects_other_recording_and_accepts_the_same_one(tmp_path, caplog):
    caplog.set_level(logging.INFO)
    music, dl = tmp_path / "music", tmp_path / "slskd"
    music.mkdir()
    dl.mkdir()
    store = Store(tmp_path / "db.sqlite3")
    library = Library(music, music / ".staging")
    original = make_music(music / "orig.m4a", 40, variant=0)
    store.remember_track(
        "deezer:1",
        {"title": "Black Water", "artists": ["FORTUNA 812"], "duration": 40, "album": "Project n(o)va"},
    )
    store.update_track(
        "deezer:1",
        status="ready",
        file=str(original),
        source="youtube",
        audio={"duration": 40, "bitrate": 128},
    )
    # "minus": other melody, same name and length. Real: the same audio at 320 kbit/s.
    minus = make_music(tmp_path / "minus.m4a", 40, variant=1)
    for src, dst in ((minus, tmp_path / "minus.mp3"), (original, tmp_path / "real.mp3")):
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-i", str(src), "-c:a", "libmp3lame", "-b:a", "320k", str(dst)],
            check=True,
        )
    fake = FakeSlskd(
        dl,
        {
            "a\\FORTUNA 812\\Black Water.mp3": tmp_path / "minus.mp3",
            "b\\FORTUNA 812 - Project\\Black Water.mp3": tmp_path / "real.mp3",
        },
    )
    server = TestServer(fake.app())
    await server.start_server()
    client = slskd.Slskd(str(server.make_url("")), "key", dl)
    upgrader = slskd.Upgrader(store, library, client, min_peers=1)
    try:
        client_download = client.download

        async def fast_download(rf, timeout=5, poll=0.01):
            return await client_download(rf, timeout, poll)

        client.download = fast_download
        result = await upgrader(type("Job", (), {"track_id": "deezer:1"})(), None)
    finally:
        await client.close()
        await server.close()
    assert result.source == "soulseek"
    assert result.source_ref == "Black Water.mp3"
    assert result.file.endswith("[deezer-1].mp3")
    assert result.audio["bitrate"] >= 300
    assert "different recording" in caplog.text  # the minus was downloaded, checked and refused
    store.close()


def test_locate_finds_names_with_brackets_outside_the_remote_folder(tmp_path):
    stored = tmp_path / "other folder" / "[2019] Black Water.mp3"
    stored.parent.mkdir()
    stored.write_bytes(b"x" * 10)
    client = slskd.Slskd("http://unused", "key", tmp_path)
    rf = slskd.RemoteFile("peer", "music\\Album\\[2019] Black Water.mp3", 10, 320, 40, True, 0)
    assert client.locate(rf) == stored


async def test_download_escapes_peer_names(tmp_path):
    source = tmp_path / "src.mp3"
    source.write_bytes(b"y" * 10)
    fake = FakeSlskd(tmp_path / "dl", {"a\\Album\\Song.mp3": source})
    server = TestServer(fake.app())
    await server.start_server()
    client = slskd.Slskd(str(server.make_url("")), "key", tmp_path / "dl")
    rf = slskd.RemoteFile("dj/one?#", "a\\Album\\Song.mp3", 10, 320, 40, True, 0)
    try:
        path = await client.download(rf, timeout=5, poll=0.01)
    finally:
        await client.close()
        await server.close()
    assert path == tmp_path / "dl" / "Album" / "Song.mp3"
    assert list(fake.transfers) == ["dj/one?#"]

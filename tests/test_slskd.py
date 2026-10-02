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


class ScriptedSlskd:
    """slskd whose search completes on the n-th poll and whose transfers end in a given state."""

    def __init__(self, polls_until_complete=2, transfer_state="Completed, Succeeded", search_status=200):
        self.polls_until_complete = polls_until_complete
        self.transfer_state = transfer_state
        self.search_status = search_status
        self.polls = 0
        self.deleted: list[str] = []
        self.keys: set[str] = set()

    def app(self):
        app = web.Application()
        app.router.add_post("/api/v0/searches", self.create)
        app.router.add_get("/api/v0/searches/{id}", self.state)
        app.router.add_get("/api/v0/searches/{id}/responses", self.responses)
        app.router.add_delete("/api/v0/searches/{id}", self.delete)
        app.router.add_post("/api/v0/transfers/downloads/{user}", self.enqueue)
        app.router.add_get("/api/v0/transfers/downloads/{user}", self.status)
        return app

    async def create(self, request):
        self.keys.add(request.headers.get("X-API-Key", ""))
        return web.json_response({"id": "s1"})

    async def state(self, request):
        self.polls += 1
        if self.search_status != 200:
            return web.Response(status=self.search_status)
        return web.json_response({"isComplete": self.polls >= self.polls_until_complete})

    async def responses(self, request):
        return web.json_response([response("peer", [rfile("x\\A\\Black Water.mp3")])])

    async def delete(self, request):
        self.deleted.append(request.match_info["id"])
        return web.Response(status=204)

    async def enqueue(self, request):
        return web.Response(status=201)

    async def status(self, request):
        files = [{"filename": "x\\A\\Song.mp3", "state": self.transfer_state}]
        return web.json_response({"directories": [{"files": files}]})


async def _client(fake, tmp_path):
    server = TestServer(fake.app())
    await server.start_server()
    return server, slskd.Slskd(str(server.make_url("")), "secret-key", tmp_path)


async def test_search_polls_until_complete_and_always_deletes_the_search(tmp_path):
    fake = ScriptedSlskd(polls_until_complete=3)
    server, client = await _client(fake, tmp_path)
    try:
        found = await client.search("FORTUNA 812 Black Water", timeout=5, poll=0.01)
        assert [r["username"] for r in found] == ["peer"]
        assert fake.polls == 3 and fake.deleted == ["s1"] and fake.keys == {"secret-key"}

        broken = ScriptedSlskd(search_status=500)
        server2, client2 = await _client(broken, tmp_path)
        try:
            with pytest.raises(slskd.SourceError, match="HTTP 500"):
                await client2.search("x", timeout=5, poll=0.01)
            assert broken.deleted == ["s1"]
        finally:
            await client2.close()
            await server2.close()
    finally:
        await client.close()
        await server.close()


async def test_a_failed_transfer_is_permanent_and_a_stuck_one_times_out(tmp_path):
    rf = slskd.RemoteFile("peer", "x\\A\\Song.mp3", 10, 320, 40, True, 0)
    server, client = await _client(ScriptedSlskd(transfer_state="Completed, Errored"), tmp_path)
    try:
        with pytest.raises(slskd.SourceError) as failed:
            await client.download(rf, timeout=5, poll=0.01)
        assert not failed.value.transient
    finally:
        await client.close()
        await server.close()
    server, client = await _client(ScriptedSlskd(transfer_state="InProgress"), tmp_path)
    try:
        with pytest.raises(slskd.SourceError, match="timed out"):
            await client.download(rf, timeout=0.1, poll=0.01)
    finally:
        await client.close()
        await server.close()


class NoNetwork:
    def __init__(self, responses=()):
        self.responses = list(responses)
        self.searched: list[str] = []

    async def search(self, text):
        self.searched.append(text)
        return self.responses


def _ready_track(store, tmp_path, bitrate, source="youtube"):
    path = tmp_path / "t.m4a"
    path.write_bytes(b"x")
    store.remember_track("deezer:1", {"title": "Black Water", "artists": ["FORTUNA 812"], "duration": 40})
    store.update_track("deezer:1", status="ready", file=str(path), source=source, audio={"bitrate": bitrate})


async def test_upgrader_leaves_good_or_unmatched_tracks_alone(tmp_path):
    store = Store(tmp_path / "db.sqlite3")
    library = Library(tmp_path, tmp_path / ".staging")
    job = type("Job", (), {"track_id": "deezer:1"})()
    try:
        client = NoNetwork()
        _ready_track(store, tmp_path, bitrate=320)
        assert (await slskd.Upgrader(store, library, client)(job, None)).source == "youtube"
        assert client.searched == []  # already good enough: no search at all

        _ready_track(store, tmp_path, bitrate=128)
        assert (await slskd.Upgrader(store, library, client)(job, None)).source == "youtube"
        assert client.searched == ["FORTUNA 812 Black Water"]  # searched, nothing acceptable
    finally:
        store.close()


async def test_upgrader_refuses_to_replace_without_a_fingerprint(tmp_path, monkeypatch):
    store = Store(tmp_path / "db.sqlite3")
    _ready_track(store, tmp_path, bitrate=128)
    found = [
        response(f"peer{i}", [rfile("x\\FORTUNA 812\\Black Water.mp3", 5000, 320, 40)]) for i in range(2)
    ]

    async def no_fpcalc(path, length=120):
        return None

    monkeypatch.setattr(slskd.audio, "fingerprint", no_fpcalc)
    upgrader = slskd.Upgrader(store, Library(tmp_path, tmp_path / ".staging"), NoNetwork(found))
    try:
        with pytest.raises(slskd.SourceError, match="fpcalc") as refused:
            await upgrader(type("Job", (), {"track_id": "deezer:1"})(), None)
        assert not refused.value.transient
    finally:
        store.close()

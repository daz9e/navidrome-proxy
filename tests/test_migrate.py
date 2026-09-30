import json
import shutil

import pytest

from navidrome_proxy import audio, migrate, tagging
from navidrome_proxy.db import Store

from .audio_fixtures import make

pytestmark = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="ffmpeg missing")


@pytest.fixture
def legacy(tmp_path):
    music = tmp_path / "music"
    music.mkdir()
    ready = make(music / "deezer-111.m4a", 25, dash=True)
    tagging.write_tags(
        ready,
        {"title": "Old Song", "artist": "TVETH / Bato", "album": "RAW", "album_artist": "TVETH", "track": 2},
    )
    (music / "deezer-111.lrc").write_text("[00:01]la")
    make(music / "Persona 3 - Aria.m4a", 25)  # not ours: ignored
    state = {
        "jobs": {
            "deezer:111": {
                "id": "deezer:111",
                "title": "Old Song",
                "artist": "TVETH / Bato",
                "status": "ready",
                "local_id": "NAV111",
                "file": "/music/deezer-111.m4a",
                "album_id": "900",
            },
            "deezer:222": {
                "id": "deezer:222",
                "title": "Never Came",
                "artist": "X",
                "status": "failed",
                "pending_star_users": {"carol": 1.0},
                "storage_reasons": ["star:carol"],
            },
            "fallback:abc": {
                "id": "fallback:abc",
                "kind": "fallback",
                "title": "Mania",
                "artist": "SchuberTEKK",
                "status": "failed",
                "pending_star_users": {"carol": 2.0},
            },
            "deezer-album:900": {"id": "deezer-album:900", "status": "idle"},
        },
        "imports": {},
    }
    state_file = tmp_path / "state.json"
    state_file.write_text(json.dumps(state))
    store = Store(tmp_path / "db.sqlite3")
    yield store, music, state_file
    store.close()


async def test_migrate_adopts_files_state_and_likes(legacy):
    store, music, state_file = legacy
    stats = await migrate.migrate_legacy(store, music, state_file)
    track = store.get_track("deezer:111")
    assert track.is_ready and track.local_id == "NAV111" and track.album_id == "900"
    assert track.artists == ["TVETH", "Bato"] and abs(track.audio["duration"] - 25) < 1
    assert [t for _, t, _ in store.pending_stars("carol")] == ["deezer:222"]
    assert store.get_job("deezer:222", "download").reason == "like:carol"
    legacy_import = store.get_import("legacy-carol")
    assert [r["title"] for r in legacy_import["rows"]] == ["Mania"]
    assert stats["files_adopted"] == 1
    again = await migrate.migrate_legacy(store, music, state_file)  # idempotent
    assert again["files_known"] == 1 and "legacy_import_rows" not in again


async def test_relayout_moves_file_and_sidecar(legacy):
    store, music, state_file = legacy
    await migrate.migrate_legacy(store, music, state_file)
    assert migrate.relayout(store, music, dry_run=True)["moved"] == 1
    assert (music / "deezer-111.m4a").exists()
    stats = migrate.relayout(store, music)
    target = music / "TVETH" / "RAW" / "1-02 - Old Song [deezer-111].m4a"
    assert stats["moved"] == 1 and target.is_file() and target.with_suffix(".lrc").is_file()
    assert store.get_track("deezer:111").file == str(target)
    assert migrate.relayout(store, music)["kept"] == 1


async def test_remux_dash(legacy):
    _, music, _ = legacy
    assert (await migrate.remux_dash(music, dry_run=True))["remuxed"] == 1
    make(music / ".deezer-111.remux.m4a", 5)  # left behind by an interrupted run
    stats = await migrate.remux_dash(music)
    assert stats == {"checked": 2, "remuxed": 1, "truncated": 0, "failed": 0}
    assert (await audio.probe(music / "deezer-111.m4a")).brand != "dash"
    assert tagging.read_tags(music / "deezer-111.m4a")["title"] == ["Old Song"]  # tags survive


async def test_repair_covers_reembeds_truncated_artwork(tmp_path):
    from aiohttp import web
    from aiohttp.test_utils import TestServer

    from navidrome_proxy.covers import Covers

    full = b"\xff\xd8\xff\xe0" + b"x" * 5000 + b"\xff\xd9"

    async def image(_):
        return web.Response(body=full, content_type="image/jpeg")

    app = web.Application()
    app.router.add_get("/c.jpg", image)
    server = TestServer(app)
    await server.start_server()
    covers = Covers(tmp_path / "covers")
    await covers.start()
    music = tmp_path / "music"
    (music / "TVETH" / "RAW").mkdir(parents=True)
    song = make(music / "TVETH" / "RAW" / "1-02 - Old Song [deezer-111].m4a", 5)
    tagging.write_tags(song, {"title": "Old Song", "artist": "TVETH"}, cover=full[:1378])
    fine = make(music / "TVETH" / "RAW" / "1-03 - Other [deezer-112].m4a", 5)
    tagging.write_tags(fine, {"title": "Other", "artist": "TVETH"}, cover=full)
    store = Store(tmp_path / "db.sqlite3")
    store.remember_track("deezer:111", {"title": "Old Song", "cover": str(server.make_url("/c.jpg"))})
    try:
        dry = await migrate.repair_covers(store, music, covers, dry_run=True)
        assert dry == {"checked": 2, "broken": 1, "repaired": 1, "unresolved": 0}
        assert tagging.embedded_cover(song) == full[:1378]
        await migrate.repair_covers(store, music, covers)
        assert tagging.embedded_cover(song) == full
        assert tagging.read_tags(song)["title"] == ["Old Song"]
        assert (await migrate.repair_covers(store, music, covers))["broken"] == 0
    finally:
        store.close()
        await covers.close()
        await server.close()


async def test_adopt_youtube_fallbacks_tags_files_from_the_catalog(tmp_path):
    from aiohttp.test_utils import TestServer

    from navidrome_proxy.covers import Covers
    from navidrome_proxy.deezer import Deezer

    from .fakes import FakeDeezer, dz_track

    fake = FakeDeezer()
    fake.add_album(
        100,
        "RAW",
        "TVETH",
        1,
        [
            dz_track(4145181542, "BURN", "тёмный принц", 100, "RAW", duration=20, position=2),
            dz_track(4145181543, "Long", "тёмный принц", 100, "RAW", duration=200, position=3),
        ],
    )
    server = TestServer(fake.app())
    await server.start_server()
    store = Store(tmp_path / "db.sqlite3")
    deezer = Deezer(store, per_second=100, base_url=str(server.make_url("")).rstrip("/"))
    await deezer.start()
    covers = Covers(tmp_path / "covers")
    await covers.start()
    music = tmp_path / "music"
    music.mkdir()
    burn = make(music / "yt-4145181542-yt.m4a", 20)
    tagging.write_tags(burn, {"title": "BURN", "artist": "тёмный принц"})
    other = make(music / "yt-4145181543-yt.m4a", 20)  # 20 s vs 200 s: another version
    store.remember_track("yt:4145181542-yt", {"title": "BURN"})  # misread by the old regex
    store.update_track("yt:4145181542-yt", status="ready", file=str(burn), local_id="ND1")
    try:
        dry = await migrate.adopt_youtube_fallbacks(store, music, deezer, covers, dry_run=True)
        assert dry == {"found": 2, "adopted": 1, "other_version": 1}
        assert "album" not in tagging.read_tags(burn)
        await migrate.adopt_youtube_fallbacks(store, music, deezer, covers)
        tags = tagging.read_tags(burn)
        assert tags["album"] == ["RAW"] and tags["tracknumber"][0].startswith("2")
        track = store.get_track("deezer:4145181542")
        assert (track.status, track.file, track.local_id) == ("ready", str(burn), "ND1")
        assert store.get_track("yt:4145181542-yt").status == "new"
        assert "album" not in tagging.read_tags(other)
        # relayout now names it like any other catalog track
        assert migrate.relayout(store, music)["moved"] == 1
        assert (music / "TVETH" / "RAW" / "1-02 - BURN [deezer-4145181542].m4a").is_file()
    finally:
        store.close()
        await covers.close()
        await deezer.close()
        await server.close()

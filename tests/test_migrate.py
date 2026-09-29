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
    stats = await migrate.remux_dash(music)
    assert stats == {"checked": 2, "remuxed": 1, "failed": 0}
    assert (await audio.probe(music / "deezer-111.m4a")).brand != "dash"
    assert tagging.read_tags(music / "deezer-111.m4a")["title"] == ["Old Song"]  # tags survive


async def test_repair_covers_reembeds_truncated_artwork(tmp_path):
    from aiohttp import web
    from aiohttp.test_utils import TestServer

    from navidrome_proxy.covers import Covers

    full = b"\xff\xd8\xff\xe0" + b"x" * 5000 + b"\xff\xd9"
    app = web.Application()
    app.router.add_get("/c.jpg", lambda _: web.Response(body=full, content_type="image/jpeg"))
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

import asyncio

import pytest
from aiohttp.test_utils import TestServer

from navidrome_proxy.db import Store
from navidrome_proxy.library import Indexer, Library, file_stem, relative_path, safe_name, track_id_from_path
from navidrome_proxy.navidrome import Navidrome
from navidrome_proxy.subsonic.params import Params
from navidrome_proxy.subsonic.response import ServerIdentity

from .fakes import FakeNavidrome, song


def test_file_stem():
    assert file_stem("deezer:42") == "deezer-42"
    assert file_stem("yt:sO4pSj466Hk") == "yt-sO4pSj466Hk"
    with pytest.raises(ValueError):
        file_stem("L1")


META = {
    "title": "Hella Quality",
    "artists": ["TVETH", "Bato"],
    "album_artist": "TVETH",
    "album": "RAW",
    "track": 3,
    "disc": 1,
}


def test_relative_path_layout():
    assert str(relative_path("deezer:1", META, ".m4a")) == "TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a"
    yt = relative_path("yt:sO4pSj466Hk", {"title": "Кошмар", "artists": ["ЮПИ"]}, "m4a")
    assert str(yt) == "ЮПИ/YouTube/Кошмар [yt-sO4pSj466Hk].m4a"
    odd = relative_path("deezer:2", {"title": "AC/DC: Why?", "artist": "..", "album": None}, ".mp3")
    assert str(odd) == "Unknown/Singles/AC DC Why [deezer-2].mp3"


def test_safe_name_limits_length():
    assert len(safe_name("x" * 500)) == 80
    assert safe_name("  ") == "Unknown"


def test_track_id_from_path_new_and_legacy():
    assert track_id_from_path("TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a") == "deezer:1"
    assert track_id_from_path("/music/deezer-4274804272.m4a") == "deezer:4274804272"
    assert track_id_from_path("ЮПИ/YouTube/Кошмар [yt-sO4pSj466Hk].m4a") == "yt:sO4pSj466Hk"
    assert track_id_from_path("Persona/Aria of the Soul.m4a") is None


def test_publish_is_atomic_and_moves_sidecars_on_format_change(tmp_path):
    lib = Library(tmp_path / "music", tmp_path / "music" / ".staging")
    staging = lib.new_staging("deezer:1")
    first = staging / "audio.m4a"
    first.write_bytes(b"one")
    dest = lib.publish("deezer:1", first, META)
    assert dest == tmp_path / "music" / "TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a"
    assert dest.read_bytes() == b"one"
    dest.with_suffix(".lrc").write_text("[00:01]x")
    upgrade = lib.new_staging("deezer:1") / "audio.mp3"
    upgrade.write_bytes(b"two")
    dest2 = lib.publish("deezer:1", upgrade, META, previous=str(dest))
    assert dest2.suffix == ".mp3" and not dest.exists()
    assert dest2.with_suffix(".lrc").read_text() == "[00:01]x"
    assert not list((tmp_path / "music").rglob("*.part"))


def test_publish_prunes_empty_folders_of_legacy_files(tmp_path):
    lib = Library(tmp_path / "music", tmp_path / "music" / ".staging")
    legacy_dir = tmp_path / "music" / "Old" / "Folder"
    legacy_dir.mkdir(parents=True)
    legacy = legacy_dir / "deezer-1.m4a"
    legacy.write_bytes(b"old")
    new = lib.new_staging("deezer:1") / "audio.m4a"
    new.write_bytes(b"new")
    lib.publish("deezer:1", new, META, previous=str(legacy))
    assert not (tmp_path / "music" / "Old").exists()
    assert (tmp_path / "music").is_dir()


def test_cleanup_staging(tmp_path):
    lib = Library(tmp_path / "m", tmp_path / "m" / ".staging")
    old = lib.new_staging("deezer:1")
    assert lib.cleanup_staging(max_age=-1) == 1
    assert not old.exists()


@pytest.fixture
async def env(tmp_path):
    fake = FakeNavidrome()
    server = TestServer(fake.app())
    await server.start_server()
    nav = Navidrome(str(server.make_url("")), ServerIdentity())
    await nav.start()
    store = Store(tmp_path / "db.sqlite3")
    yield fake, nav, store
    await nav.close()
    await server.close()
    store.close()


async def test_indexer_scans_once_and_maps_by_file_name(env):
    fake, nav, store = env
    mapped = []

    async def on_mapped(track):
        mapped.append((track.id, track.local_id))

    music = store.path.parent / "music"
    music.mkdir()
    for i in (1, 2):
        f = music / f"deezer-{i}.m4a"
        f.write_bytes(b"x" * (1000 + i))
        store.remember_track(f"deezer:{i}", {"title": "Same Title", "artists": ["A"], "duration": 200})
        store.update_track(f"deezer:{i}", status="ready", file=str(f))

    def scan(fake):
        # Same title and a synthetic path (Navidrome hides real paths): the exact
        # byte size is what tells the two files apart.
        fake.songs["L1"] = song("L1", "Same Title", "A", path="A/Album/01-01 - Same Title.m4a", size=1001)
        fake.songs["L2"] = song("L2", "Same Title", "A", path="A/Album/01-01 - Same Title.m4a", size=1002)

    fake.on_scan = scan
    creds = Params.parse("u=alice&p=alicepw")
    indexer = Indexer(store, nav, lambda: creds, debounce=0.05, on_mapped=on_mapped)
    indexer.start()
    indexer.notify()
    indexer.notify()
    for _ in range(100):
        if len(mapped) == 2:
            break
        await asyncio.sleep(0.05)
    await indexer.stop()
    assert sorted(mapped) == [("deezer:1", "L1"), ("deezer:2", "L2")]
    assert fake.scans == 1


async def test_find_local_id_without_credentials_is_none(env):
    _, nav, store = env
    store.remember_track("deezer:1", {"title": "x"})
    store.update_track("deezer:1", status="ready", file="/music/deezer-1.m4a")
    indexer = Indexer(store, nav, lambda: None)
    assert await indexer.resolve(store.ready_without_local_id()) == 0

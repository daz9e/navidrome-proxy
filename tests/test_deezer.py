import asyncio
import time

import pytest
from aiohttp.test_utils import TestServer

from navidrome_proxy import deezer as dz
from navidrome_proxy.db import Store

from .fakes import FakeDeezer, dz_track


@pytest.fixture
async def deezer(tmp_path):
    fake = FakeDeezer()
    fake.add_album(
        100,
        "RAW",
        "TVETH",
        1,
        [
            dz_track(4250555171, "Hella Quality", "TVETH", 100, "RAW", contributors=["TVETH", "Bato"]),
            dz_track(2, "Second", "TVETH", 100, "RAW", position=2),
        ],
    )
    server = TestServer(fake.app())
    await server.start_server()
    store = Store(tmp_path / "db.sqlite3")
    client = dz.Deezer(store, per_second=100, base_url=str(server.make_url("")).rstrip("/"))
    await client.start()
    yield fake, client, store
    await client.close()
    await server.close()
    store.close()


def test_track_meta_keeps_every_contributor():
    t = dz_track(1, "Hella Quality", "TVETH", contributors=["TVETH", "Bato"])
    meta = dz.track_meta(
        t, {"nb_tracks": 12, "genres": {"data": [{"name": "Rap"}]}, "artist": {"name": "TVETH"}}
    )
    assert meta["artists"] == ["TVETH", "Bato"]
    assert meta["artist"] == "TVETH, Bato"
    assert meta["album_artist"] == "TVETH"
    assert meta["year"] == 2020 and meta["genre"] == "Rap" and meta["track_total"] == 12
    assert meta["cover"].endswith("/md5100/1000x1000-000000-80-0-0.jpg")
    assert meta["full"] is True
    assert dz.track_meta(dz_track(1, "x", "y"))["full"] is False


async def test_search_and_full_track_meta(deezer):
    _, client, _ = deezer
    rows = await client.search_tracks("hella quality")
    assert [r["id"] for r in rows] == [4250555171]
    meta = await client.full_track_meta(4250555171)
    assert meta["artists"] == ["TVETH", "Bato"]
    assert meta["genre"] == "Rap"


async def test_single_flight_and_persistent_cache(deezer):
    fake, client, _ = deezer
    await asyncio.gather(*(client.album(100) for _ in range(5)))
    assert sum(1 for h in fake.hits if h == "/album/100") == 1
    client.memory = dz.LRU(10)
    await client.album(100)
    assert sum(1 for h in fake.hits if h == "/album/100") == 1  # served from SQLite
    assert client.cached_album_tracks(100) is None
    await client.album_tracks(100)
    assert [t["id"] for t in client.cached_album_tracks(100)] == [4250555171, 2]


async def test_quota_error_is_retried(deezer):
    fake, client, _ = deezer
    fake.quota_failures = 2
    assert (await client.artist(1))["name"] == "TVETH"


async def test_not_found_raises(deezer):
    _, client, _ = deezer
    with pytest.raises(dz.DeezerError):
        await client.track(999)


async def test_rate_limiter_spaces_requests():
    limiter = dz.RateLimiter(per_second=20, burst=2)
    start = time.monotonic()
    for _ in range(6):
        await limiter.acquire()
    assert time.monotonic() - start >= 0.18


async def test_background_callers_leave_a_reserve_for_interactive_ones():
    limiter = dz.RateLimiter(per_second=5, burst=6, reserve=4)
    order = []

    async def bg(i):
        await limiter.acquire(True)
        order.append(f"bg{i}")

    async def fg():
        await limiter.acquire(False)
        order.append("fg")

    tasks = [asyncio.create_task(bg(i)) for i in range(4)]
    await asyncio.sleep(0.05)
    await asyncio.wait_for(fg(), timeout=0.2)  # served from the reserve, no waiting
    assert order[:2] == ["bg0", "bg1"] and "fg" in order[:3]
    for t in tasks:
        t.cancel()

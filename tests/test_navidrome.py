import hashlib

import pytest
from aiohttp import web
from aiohttp.test_utils import TestServer

from navidrome_proxy.navidrome import Navidrome, UpstreamError, UpstreamUnavailable
from navidrome_proxy.subsonic.params import Params
from navidrome_proxy.subsonic.response import ServerIdentity

from .fakes import FakeNavidrome, song


def token(user_pw: str, salt: str = "abc") -> str:
    return f"t={hashlib.md5((user_pw + salt).encode()).hexdigest()}&s={salt}"


@pytest.fixture
async def upstream():
    fake = FakeNavidrome()
    fake.songs["L1"] = song("L1", "Starboy", "The Weeknd")
    server = TestServer(fake.app())
    await server.start_server()
    nav = Navidrome(str(server.make_url("")), ServerIdentity(), auth_ttl=60)
    await nav.start()
    yield fake, nav
    await nav.close()
    await server.close()


async def test_authenticate_caches_and_remembers_credentials(upstream):
    fake, nav = upstream
    params = Params.parse(f"u=alice&{token('alicepw')}&v=1.16.1&c=test&f=json")
    assert (await nav.authenticate(params)).ok
    assert (await nav.authenticate(params)).ok
    assert sum(1 for m, _ in fake.requests if m == "ping") == 1
    assert nav.credentials_for("alice").get("t")
    assert nav.identity.server_version == "0.63.2 (fake)"


async def test_authenticate_rejects_wrong_and_missing(upstream):
    _, nav = upstream
    bad = await nav.authenticate(Params.parse("u=alice&p=nope"))
    assert not bad.ok and bad.code == 40
    missing = await nav.authenticate(Params.parse("u=alice"))
    assert not missing.ok and missing.code == 10
    assert nav.credentials_for("alice") is None


async def test_call_returns_root_or_raises(upstream):
    _, nav = upstream
    params = Params.parse("u=bob&p=bobpw")
    root = await nav.call("getSong", params, id="L1")
    assert root["song"]["title"] == "Starboy"
    with pytest.raises(UpstreamError) as exc:
        await nav.call("getSong", params, id="missing")
    assert exc.value.code == 70


async def test_unavailable_upstream():
    nav = Navidrome("http://127.0.0.1:9", ServerIdentity())
    await nav.start()
    try:
        with pytest.raises(UpstreamUnavailable):
            await nav.call("ping", Params.parse("u=a&p=b"))
        result = await nav.authenticate(Params.parse("u=a&p=b"))
        assert result.unavailable and not result.ok
    finally:
        await nav.close()


async def test_passthrough_streams_range_and_forwards_host(upstream, aiohttp_client):
    _, nav = upstream

    async def handler(request):
        return await nav.passthrough(request)

    app = web.Application()
    app.router.add_route("*", "/{tail:.*}", handler)
    client = await aiohttp_client(app)
    resp = await client.get("/rest/stream.view?u=alice&p=alicepw&id=L1", headers={"Range": "bytes=10-19"})
    assert resp.status == 206
    assert resp.headers["Content-Range"] == "bytes 10-19/50000"
    assert resp.headers["Content-Length"] == "10"
    assert len(await resp.read()) == 10
    ui = await client.get("/app/", headers={"Host": "music.example.org"})
    assert ui.headers["X-Seen-Host"] == "music.example.org"
    assert await ui.text() == "navidrome ui /app/"
    plain = await client.get("/app/", skip_auto_headers=["Accept-Encoding"])
    assert plain.headers["X-Seen-Encoding"] == "identity"
    gz = await client.get("/app/", headers={"Accept-Encoding": "gzip"})
    assert gz.headers["X-Seen-Encoding"] == "gzip"

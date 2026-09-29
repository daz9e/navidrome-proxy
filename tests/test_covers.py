import asyncio

from aiohttp import web
from aiohttp.test_utils import TestServer

from navidrome_proxy.covers import Covers, is_complete_image

JPEG = b"\xff\xd8\xff\xe0" + bytes(range(256)) * 400 + b"\xff\xd9"


async def _server(handler):
    app = web.Application()
    app.router.add_get("/{name}", handler)
    server = TestServer(app)
    await server.start_server()
    return server


async def slow(request):
    """Like a CDN: the body arrives in several network chunks."""
    resp = web.StreamResponse(headers={"Content-Type": "image/jpeg"})
    body = JPEG[:-100] if request.match_info["name"] == "cut.jpg" else JPEG
    resp.content_length = len(body)
    await resp.prepare(request)
    for i in range(0, len(body), 8192):
        await resp.write(body[i : i + 8192])
        await asyncio.sleep(0.005)
    return resp


async def test_reads_the_whole_image_and_caches_it(tmp_path):
    server = await _server(slow)
    covers = Covers(tmp_path)
    await covers.start()
    try:
        url = str(server.make_url("/a.jpg"))
        data, ctype = await covers.get(url)
        assert data == JPEG and ctype == "image/jpeg"
        await server.close()
        assert (await covers.get(url))[0] == JPEG  # from disk
    finally:
        await covers.close()
        await server.close()


async def test_short_body_is_not_cached(tmp_path):
    server = await _server(slow)
    covers = Covers(tmp_path)
    await covers.start()
    try:
        assert await covers.get(str(server.make_url("/cut.jpg"))) is None
        assert not list(tmp_path.rglob("*.img"))
    finally:
        await covers.close()
        await server.close()


async def test_truncated_cache_entry_is_downloaded_again(tmp_path):
    server = await _server(slow)
    covers = Covers(tmp_path)
    await covers.start()
    try:
        url = str(server.make_url("/b.jpg"))
        path = covers._path(url)
        path.parent.mkdir(parents=True)
        path.write_bytes(JPEG[:1378])
        assert (await covers.get(url))[0] == JPEG
        assert path.read_bytes() == JPEG
    finally:
        await covers.close()
        await server.close()


def test_is_complete_image():
    assert is_complete_image(JPEG)
    assert not is_complete_image(JPEG[:1378])
    assert not is_complete_image(b"")
    png = b"\x89PNG\r\n\x1a\n" + b"x" * 100 + b"\x00\x00\x00\x00IEND\xaeB`\x82"
    assert is_complete_image(png) and not is_complete_image(png[:50])

import shutil

import pytest
from aiohttp import web
from mutagen.mp4 import MP4

from navidrome_proxy import lyrics, tagging

from .audio_fixtures import make

pytestmark = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="ffmpeg missing")

META = {
    "title": "Hella Quality",
    "artist": "TVETH, Bato",
    "artists": ["TVETH", "Bato"],
    "album": "RAW",
    "album_artist": "TVETH",
    "track": 3,
    "track_total": 12,
    "disc": 1,
    "year": 2026,
    "genre": "Rap",
    "isrc": "QZ1",
    "explicit": True,
    "duration": 98,
}
JPEG = b"\xff\xd8\xff\xe0" + b"0" * 100


def test_m4a_tags(tmp_path):
    f = make(tmp_path / "a.m4a", 25)
    tagging.write_tags(f, META, cover=JPEG)
    tags = MP4(f).tags
    assert tags["\xa9ART"] == ["TVETH, Bato"]
    assert [bytes(x) for x in tags["----:com.apple.iTunes:ARTISTS"]] == [b"TVETH", b"Bato"]
    assert tags["aART"] == ["TVETH"] and tags["trkn"] == [(3, 12)] and tags["\xa9day"] == ["2026"]
    assert bytes(tags["covr"][0]) == JPEG


def test_retagging_replaces_everything(tmp_path):
    f = make(tmp_path / "a.m4a", 25)
    tagging.write_tags(f, META)
    tagging.write_tags(f, {"title": "Only Title", "artist": "X"})
    tags = MP4(f).tags
    assert tags["\xa9nam"] == ["Only Title"]
    assert "\xa9alb" not in tags and "\xa9gen" not in tags


def test_mp3_tags(tmp_path):
    f = make(tmp_path / "a.mp3", 25, codec="mp3")
    tagging.write_tags(f, META, cover=JPEG)
    read = tagging.read_tags(f)
    assert read["title"] == ["Hella Quality"]
    assert read["artist"] == ["TVETH, Bato"]
    assert read["albumartist"] == ["TVETH"]
    assert read["tracknumber"] == ["3/12"]


def test_unsupported_suffix(tmp_path):
    with pytest.raises(ValueError):
        tagging.write_tags(tmp_path / "x.flac", META)


async def test_lyrics_prefers_synced(aiohttp_client, tmp_path):
    async def handler(request):
        assert request.query["artist_name"] == "TVETH"
        return web.json_response({"syncedLyrics": "[00:01.00] hi", "plainLyrics": "hi"})

    app = web.Application()
    app.router.add_get("/api/get", handler)
    client = await aiohttp_client(app)
    found = await lyrics.fetch(client.session, META, str(client.make_url("/api/get")))
    assert found == ("lrc", "[00:01.00] hi")
    path = lyrics.save(tmp_path / "deezer-1.m4a", found)
    assert path.name == "deezer-1.lrc"


async def test_lyrics_missing_is_none(aiohttp_client):
    async def handler(request):
        return web.json_response({"code": 404}, status=404)

    app = web.Application()
    app.router.add_get("/api/get", handler)
    client = await aiohttp_client(app)
    assert await lyrics.fetch(client.session, META, str(client.make_url("/api/get"))) is None

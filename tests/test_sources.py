import threading
from pathlib import Path

import pytest

from navidrome_proxy import matching, sources

YTM_ROWS = [
    {
        "resultType": "song",
        "videoId": "YUyG_nM3Rig",
        "title": "порезы на руках",
        "artists": [{"name": "FORTUNA 812"}, {"name": "elox1m"}],
        "album": {"name": "x"},
        "duration_seconds": 144,
    },
    {
        "resultType": "song",
        "videoId": "Q1QvyIrUzz0",
        "title": "Порезы на руках",
        "artists": [{"name": "Юпи"}],
        "album": {"name": "Порезы на руках"},
        "duration_seconds": 124,
        "thumbnails": [{"url": "https://t/1"}],
    },
    {
        "resultType": "song",
        "videoId": "gvniJHIecH4",
        "title": "Порезы на руках (sped up)",
        "artists": [{"name": "Юпи"}],
        "duration_seconds": 124,
    },
]
WANTED = matching.Wanted("Порезы на руках", ["Юпи"], duration=124)


async def test_youtube_music_is_preferred_and_filtered():
    calls = []

    def flat(query):
        calls.append(query)
        return []

    finder = sources.Sources(ytmusic_search=lambda q, f, n: YTM_ROWS, flat_search=flat)
    found = await finder.find(WANTED)
    assert [c.ref for c in found] == ["Q1QvyIrUzz0"]
    assert found[0].official and found[0].album == "Порезы на руках"
    assert calls == []  # fallbacks are not searched when YouTube Music succeeds


async def test_fallback_searches_youtube_and_soundcloud_in_parallel():
    def flat(query):
        if query.startswith("ytsearch"):
            return [
                {
                    "id": "AAAAAAAAAAA",
                    "title": "Юпи - Порезы на руках (минус)",
                    "channel": "x",
                    "duration": 124,
                }
            ]
        return [
            {
                "id": "2398309779",
                "url": "https://api.soundcloud.com/tracks/2398309779",
                "title": "Юпи - Порезы на руках",
                "uploader": "ima",
                "duration": 121.8,
            }
        ]

    finder = sources.Sources(ytmusic_search=lambda q, f, n: [], flat_search=flat)
    found = await finder.find(WANTED)
    assert [(c.source, c.ref) for c in found] == [("soundcloud", "2398309779")]


async def test_search_failure_is_not_fatal():
    def boom(*a):
        raise RuntimeError("network down")

    finder = sources.Sources(ytmusic_search=boom, flat_search=boom)
    assert await finder.find(WANTED) == []


async def test_excluded_refs_are_skipped():
    finder = sources.Sources(ytmusic_search=lambda q, f, n: YTM_ROWS, flat_search=lambda q: [])
    assert await finder.find(WANTED, exclude={"Q1QvyIrUzz0"}) == []


async def test_download_passes_format_and_propagates_errors(tmp_path):
    seen = {}

    def fake(url, fmt, dest, cancel, deadline):
        seen.update(url=url, fmt=fmt)
        (dest / "source.m4a").parent.mkdir(parents=True, exist_ok=True)
        p = dest / "source.m4a"
        p.write_bytes(b"x")
        return p

    cand = sources.Candidate("soundcloud", "https://sc/1", "1", "t", "u", 1)
    path = await sources.download(cand, tmp_path / "job", downloader=fake)
    assert path == tmp_path / "job" / "source.m4a"
    assert seen["fmt"] == sources.SOUNDCLOUD_FORMAT

    def failing(*a):
        raise sources.SourceError("DRM protected", transient=False)

    with pytest.raises(sources.SourceError) as exc:
        await sources.download(cand, tmp_path / "job2", downloader=failing)
    assert not exc.value.transient


def test_blocking_download_honours_cancel(tmp_path, monkeypatch):
    import yt_dlp

    class FakeYDL:
        def __init__(self, opts):
            self.opts = opts

        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

        def extract_info(self, url, download):
            for hook in self.opts["progress_hooks"]:
                hook({"status": "downloading"})
            return {}

    monkeypatch.setattr(yt_dlp, "YoutubeDL", FakeYDL)
    cancel = threading.Event()
    cancel.set()
    with pytest.raises(sources.Cancelled):
        sources._download_blocking("u", "f", Path(tmp_path), cancel, deadline=1e18)


BODY = bytes(range(256)) * 4000  # ~1 MB


async def _range_server(status=206):
    from aiohttp import web
    from aiohttp.test_utils import TestServer

    seen = []

    async def handle(request):
        first, last = (int(x) for x in request.headers["Range"].removeprefix("bytes=").split("-"))
        seen.append((first, last))
        if status != 206:
            return web.Response(status=status)
        return web.Response(status=206, body=BODY[first : last + 1])

    app = web.Application()
    app.router.add_get("/audio", handle)
    server = TestServer(app)
    await server.start_server()
    return server, seen


async def test_direct_fetch_joins_parallel_ranges(tmp_path):
    server, seen = await _range_server()
    try:
        info = {
            "url": str(server.make_url("/audio")),
            "protocol": "https",
            "filesize": len(BODY),
            "ext": "m4a",
        }
        path = await sources._fetch_direct(info, tmp_path, deadline=__import__("time").monotonic() + 30)
        assert path.name == "source.m4a" and path.read_bytes() == BODY
        assert len(seen) == 4
    finally:
        await server.close()


async def test_download_falls_back_to_ytdlp_when_the_direct_fetch_is_refused(tmp_path, monkeypatch):
    server, _ = await _range_server(status=403)
    info = {"url": str(server.make_url("/audio")), "protocol": "https", "filesize": len(BODY), "ext": "m4a"}
    monkeypatch.setattr(sources, "_resolve_blocking", lambda url, fmt: info)

    def ytdlp(url, fmt, dest, cancel, deadline):
        dest.mkdir(parents=True, exist_ok=True)
        (dest / "source.webm").write_bytes(b"x")
        return dest / "source.webm"

    monkeypatch.setattr(sources, "_download_blocking", ytdlp)
    cand = sources.Candidate("youtube", "https://youtube.com/watch?v=x", "x", "t", "u", 1.0)
    try:
        assert (await sources.download(cand, tmp_path / "job")).name == "source.webm"
    finally:
        await server.close()


async def test_direct_fetch_needs_a_progressive_url(tmp_path):
    with pytest.raises(sources.SourceError):
        await sources._fetch_direct({"url": "https://x/m.m3u8", "protocol": "m3u8_native"}, tmp_path, 0)


async def test_direct_fetch_stops_on_cancel_without_falling_back(tmp_path, monkeypatch):
    server, _ = await _range_server()
    info = {"url": str(server.make_url("/audio")), "protocol": "https", "filesize": len(BODY), "ext": "m4a"}
    monkeypatch.setattr(sources, "_resolve_blocking", lambda url, fmt: info)
    monkeypatch.setattr(sources, "_download_blocking", lambda *a: pytest.fail("yt-dlp must not run"))
    cancel = threading.Event()
    cancel.set()
    cand = sources.Candidate("youtube", "https://youtube.com/watch?v=x", "x", "t", "u", 1.0)
    try:
        with pytest.raises(sources.Cancelled):
            await sources.download(cand, tmp_path / "job", cancel=cancel)
    finally:
        await server.close()

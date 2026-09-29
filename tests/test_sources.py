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

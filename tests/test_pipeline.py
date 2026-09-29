import shutil

import pytest
from aiohttp.test_utils import TestServer
from mutagen.mp4 import MP4

from navidrome_proxy import sources
from navidrome_proxy.config import Settings
from navidrome_proxy.covers import Covers
from navidrome_proxy.db import Store
from navidrome_proxy.deezer import Deezer
from navidrome_proxy.identity import Identity
from navidrome_proxy.library import Library
from navidrome_proxy.pipeline import Pipeline

from .audio_fixtures import make
from .fakes import FakeDeezer, dz_track

pytestmark = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="ffmpeg missing")


class FakeFinder:
    def __init__(self, candidates):
        self.candidates = candidates
        self.calls = 0

    async def find(self, wanted, exclude=None):
        self.calls += 1
        return list(self.candidates)


class FakeDownloads:
    """Produces synthetic audio of a chosen length per candidate ref."""

    def __init__(self, lengths):
        self.lengths = lengths
        self.fetched = []

    async def __call__(self, cand, dest, timeout=300, cancel=None):
        self.fetched.append(cand.ref)
        length = self.lengths[cand.ref]
        if length is None:
            raise sources.SourceError("HTTP Error 403", transient=True)
        return make(dest / "source.m4a", length, dash=True)


def cand(ref, source="youtube"):
    return sources.Candidate(source, f"https://x/{ref}", ref, "Hella Quality", "TVETH - Topic", 30)


@pytest.fixture
async def world(tmp_path):
    fake = FakeDeezer()
    fake.add_album(
        100,
        "RAW",
        "TVETH",
        1,
        [
            dz_track(
                1,
                "Hella Quality",
                "TVETH",
                100,
                "RAW",
                duration=30,
                contributors=["TVETH", "Bato"],
                position=3,
            ),
        ],
    )
    server = TestServer(fake.app())
    await server.start_server()
    store = Store(tmp_path / "db.sqlite3")
    deezer = Deezer(store, per_second=100, base_url=str(server.make_url("")).rstrip("/"))
    await deezer.start()
    covers = Covers(tmp_path / "covers")
    await covers.start()
    settings = Settings.from_env(
        {"MUSIC_DIR": str(tmp_path / "music"), "DATA_DIR": str(tmp_path), "LYRICS": "false"}
    )
    library = Library(settings.music_dir, settings.staging_dir)

    def build(candidates, lengths):
        finder = FakeFinder(candidates)
        downloads = FakeDownloads(lengths)
        return (
            Pipeline(
                settings, store, Identity(store), deezer, finder, library, None, covers, downloader=downloads
            ),
            finder,
            downloads,
        )

    yield store, build, settings
    await covers.close()
    await deezer.close()
    await server.close()
    store.close()


async def test_download_tags_and_publishes_in_library_layout(world):
    store, build, settings = world
    store.remember_track("deezer:1", {"title": "Hella Quality", "artist": "TVETH", "duration": 30})
    pipeline, _, _ = build([cand("dEvrb6N70kA")], {"dEvrb6N70kA": 30})
    track = await pipeline.download("deezer:1")
    assert track.status == "ready" and track.source == "youtube"
    path = settings.music_dir / "TVETH/RAW/1-03 - Hella Quality [deezer-1].m4a"
    assert track.file == str(path) and path.is_file()
    tags = MP4(path).tags
    assert tags["\xa9ART"] == ["TVETH, Bato"] and tags["\xa9alb"] == ["RAW"]
    assert track.audio["codec"] == "aac" and track.audio["brand"] != "dash"
    assert abs(track.audio["duration"] - 30) < 0.5
    # The YouTube source is now an alias of the Deezer track.
    assert pipeline.identity.canonical("yt:dEvrb6N70kA") == "deezer:1"
    assert not list(settings.staging_dir.iterdir())


async def test_wrong_duration_candidate_is_skipped_for_the_next(world):
    store, build, _ = world
    store.remember_track("deezer:1", {"title": "Hella Quality", "artist": "TVETH", "duration": 30})
    pipeline, _, downloads = build(
        [cand("AAAAAAAAAAA"), cand("BBBBBBBBBBB")], {"AAAAAAAAAAA": 60, "BBBBBBBBBBB": 31}
    )
    track = await pipeline.download("deezer:1")
    assert downloads.fetched == ["AAAAAAAAAAA", "BBBBBBBBBBB"]
    assert track.source_ref == "BBBBBBBBBBB"


async def test_all_candidates_failing_transiently_is_retryable(world):
    store, build, _ = world
    store.remember_track("deezer:1", {"title": "Hella Quality", "artist": "TVETH", "duration": 30})
    pipeline, _, _ = build([cand("AAAAAAAAAAA")], {"AAAAAAAAAAA": None})
    with pytest.raises(sources.SourceError) as exc:
        await pipeline.download("deezer:1")
    assert exc.value.transient


async def test_no_candidates_is_permanent(world):
    store, build, _ = world
    store.remember_track("deezer:1", {"title": "Hella Quality", "artist": "TVETH", "duration": 30})
    pipeline, _, _ = build([], {})
    with pytest.raises(sources.SourceError) as exc:
        await pipeline.download("deezer:1")
    assert not exc.value.transient


async def test_same_recording_under_another_id_is_not_downloaded_again(world):
    store, build, _ = world
    store.remember_track(
        "sc:77",
        {"title": "Hella Quality", "artists": ["TVETH"], "duration": 31, "url": "https://soundcloud/77"},
    )
    pipeline, finder, downloads = build([], {"77": 31})
    first = await pipeline.download("sc:77")
    assert first.id == "sc:77" and downloads.fetched == ["77"]
    store.remember_track("deezer:1", {"title": "Hella Quality", "artist": "TVETH", "duration": 30})
    again = await pipeline.download("deezer:1")
    assert again.id == "sc:77"
    assert downloads.fetched == ["77"] and finder.calls == 0
    assert pipeline.identity.canonical("deezer:1") == "sc:77"


async def test_youtube_track_downloads_its_own_video(world):
    store, build, settings = world
    store.remember_track("yt:sO4pSj466Hk", {"title": "Кошмар", "artists": ["ЮПИ"], "duration": 30})
    pipeline, finder, downloads = build([], {"sO4pSj466Hk": 30})
    track = await pipeline.download("yt:sO4pSj466Hk")
    assert downloads.fetched == ["sO4pSj466Hk"] and finder.calls == 0
    assert track.file.endswith("ЮПИ/YouTube/Кошмар [yt-sO4pSj466Hk].m4a")

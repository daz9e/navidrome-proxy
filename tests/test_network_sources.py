"""Live checks against YouTube Music / SoundCloud. Run with ``pytest -m network``."""

import pytest

from navidrome_proxy import matching, sources

pytestmark = pytest.mark.network


@pytest.mark.parametrize(
    ("title", "artists", "duration", "expected"),
    [
        ("Порезы на руках", ["Юпи"], 124, "Q1QvyIrUzz0"),
        ("Black Water", ["FORTUNA 812"], 177, "bQqZdPDwgcU"),
        ("Hella Quality", ["TVETH", "Bato"], 98, "dEvrb6N70kA"),
    ],
)
async def test_real_youtube_music_pick(title, artists, duration, expected):
    found = await sources.Sources().find(matching.Wanted(title, artists, duration=duration))
    assert found and found[0].ref == expected


async def test_real_download_is_fast(tmp_path):
    import time

    cand = sources.Candidate(
        "youtube",
        "https://music.youtube.com/watch?v=Q1QvyIrUzz0",
        "Q1QvyIrUzz0",
        "Порезы на руках",
        "Юпи",
        124,
    )
    start = time.monotonic()
    path = await sources.download(cand, tmp_path)
    assert path.stat().st_size > 1_000_000
    assert time.monotonic() - start < 30

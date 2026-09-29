from pathlib import Path

from navidrome_proxy.config import Settings


def test_defaults_are_sane():
    s = Settings.from_env({})
    assert s.port == 8084
    assert s.background_workers == 5
    assert s.db_path == Path("/data/proxy.sqlite3")
    assert s.staging_dir.parent == s.music_dir
    assert not s.slskd_enabled


def test_env_overrides_and_bad_values_fall_back():
    s = Settings.from_env(
        {
            "PORT": "9000",
            "NAVIDROME_URL": "http://nd:4533/",
            "BACKGROUND_WORKERS": "0",
            "STREAM_WAIT_SECONDS": "oops",
            "COMPLETE_LOCAL_ALBUMS": "no",
            "SLSKD_URL": "http://slskd:5030",
            "SLSKD_API_KEY": "k",
        }
    )
    assert s.port == 9000
    assert s.navidrome_url == "http://nd:4533"
    assert s.background_workers == 1
    assert s.stream_wait_seconds == 120.0
    assert s.complete_local_albums is False
    assert s.slskd_enabled

"""Runtime settings, read once from the environment."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path


def _bool(value: str | None, default: bool) -> bool:
    if value is None or value.strip() == "":
        return default
    return value.strip().lower() in ("1", "true", "yes", "on")


def _int(value: str | None, default: int) -> int:
    try:
        return int(value) if value not in (None, "") else default
    except ValueError:
        return default


def _float(value: str | None, default: float) -> float:
    try:
        return float(value) if value not in (None, "") else default
    except ValueError:
        return default


@dataclass(frozen=True)
class Settings:
    """All tunables of the proxy.

    Paths are the paths *inside* the proxy's own filesystem; the music directory
    must be the same directory Navidrome scans (``ND_MUSICFOLDER``).
    """

    host: str = "0.0.0.0"
    port: int = 8084
    navidrome_url: str = "http://127.0.0.1:4533"
    # Optional Navidrome admin account for background work (startScan needs admin;
    # without it the proxy uses any credentials it recently saw and relies on
    # Navidrome's file watcher).
    navidrome_user: str = ""
    navidrome_password: str = ""
    music_dir: Path = Path("/music")
    data_dir: Path = Path("/data")
    slskd_url: str = ""
    slskd_api_key: str = ""
    slskd_downloads_dir: Path = Path("/downloads-slskd")

    background_workers: int = 5
    priority_workers: int = 16
    stream_wait_seconds: float = 120.0
    auth_cache_seconds: float = 300.0
    search_cache_seconds: float = 120.0
    search_budget_seconds: float = 4.0
    deezer_requests_per_second: float = 8.0
    scan_debounce_seconds: float = 3.0
    duration_tolerance_seconds: float = 10.0

    external_songs: int = 25
    external_albums: int = 10
    external_artists: int = 5
    youtube_songs: int = 3

    complete_local_albums: bool = True
    prefetch_next_tracks: int = 2
    slskd_upgrades: bool = True
    slskd_min_peers: int = 2
    lyrics: bool = True
    log_level: str = "INFO"

    @property
    def db_path(self) -> Path:
        return self.data_dir / "proxy.sqlite3"

    @property
    def staging_dir(self) -> Path:
        # Staging must live on the same filesystem as the music directory so the
        # final publish is an atomic rename.
        return self.music_dir / ".navidrome-proxy-staging"

    @property
    def cache_dir(self) -> Path:
        return self.data_dir / "cache"

    @classmethod
    def from_env(cls, env: dict[str, str] | None = None) -> Settings:
        e = dict(os.environ if env is None else env)
        d = cls()
        return cls(
            host=e.get("HOST", d.host),
            port=_int(e.get("PORT"), d.port),
            navidrome_url=e.get("NAVIDROME_URL", d.navidrome_url).rstrip("/"),
            navidrome_user=e.get("NAVIDROME_USER", d.navidrome_user),
            navidrome_password=e.get("NAVIDROME_PASSWORD", d.navidrome_password),
            music_dir=Path(e.get("MUSIC_DIR", str(d.music_dir))),
            data_dir=Path(e.get("DATA_DIR", str(d.data_dir))),
            slskd_url=e.get("SLSKD_URL", d.slskd_url).rstrip("/"),
            slskd_api_key=e.get("SLSKD_API_KEY", d.slskd_api_key),
            slskd_downloads_dir=Path(e.get("SLSKD_DOWNLOADS_DIR", str(d.slskd_downloads_dir))),
            background_workers=max(1, _int(e.get("BACKGROUND_WORKERS"), d.background_workers)),
            priority_workers=max(1, _int(e.get("PRIORITY_WORKERS"), d.priority_workers)),
            stream_wait_seconds=_float(e.get("STREAM_WAIT_SECONDS"), d.stream_wait_seconds),
            auth_cache_seconds=_float(e.get("AUTH_CACHE_SECONDS"), d.auth_cache_seconds),
            search_cache_seconds=_float(e.get("SEARCH_CACHE_SECONDS"), d.search_cache_seconds),
            search_budget_seconds=_float(e.get("SEARCH_BUDGET_SECONDS"), d.search_budget_seconds),
            deezer_requests_per_second=_float(
                e.get("DEEZER_REQUESTS_PER_SECOND"), d.deezer_requests_per_second
            ),
            scan_debounce_seconds=_float(e.get("SCAN_DEBOUNCE_SECONDS"), d.scan_debounce_seconds),
            duration_tolerance_seconds=_float(
                e.get("DURATION_TOLERANCE_SECONDS"), d.duration_tolerance_seconds
            ),
            external_songs=_int(e.get("EXTERNAL_SONGS"), d.external_songs),
            external_albums=_int(e.get("EXTERNAL_ALBUMS"), d.external_albums),
            external_artists=_int(e.get("EXTERNAL_ARTISTS"), d.external_artists),
            youtube_songs=_int(e.get("YOUTUBE_SONGS"), d.youtube_songs),
            complete_local_albums=_bool(e.get("COMPLETE_LOCAL_ALBUMS"), d.complete_local_albums),
            prefetch_next_tracks=_int(e.get("PREFETCH_NEXT_TRACKS"), d.prefetch_next_tracks),
            slskd_upgrades=_bool(e.get("SLSKD_UPGRADES"), d.slskd_upgrades),
            slskd_min_peers=max(1, _int(e.get("SLSKD_MIN_PEERS"), d.slskd_min_peers)),
            lyrics=_bool(e.get("LYRICS"), d.lyrics),
            log_level=e.get("LOG_LEVEL", d.log_level).upper(),
        )

    @property
    def slskd_enabled(self) -> bool:
        return bool(self.slskd_url and self.slskd_api_key)

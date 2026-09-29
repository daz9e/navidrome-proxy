# navidrome-proxy

An OpenSubsonic proxy in front of Navidrome. Clients (Arpeggi or any other) connect to it
as if it were Navidrome. Search covers all music, and whatever you play, like or add to a
playlist is downloaded into your library.

- **Search:** your library + Deezer + YouTube Music. Searching never downloads anything.
- **Play / like / playlist:** the track is fetched from YouTube Music (YouTube and
  SoundCloud as fallbacks), tagged with Deezer metadata and indexed by Navidrome. Likes,
  playlist additions and scrobbles end up in Navidrome itself.
- **One song, one id:** once downloaded, the Navidrome song is shown everywhere. Albums
  don't split, and local albums are completed with their missing tracks.
- **Queue:** plays and likes skip the queue; background work runs 5 at a time.
- **Quality:** liked tracks are upgraded from Soulseek, but only when the audio
  fingerprint matches the original (no instrumental "minus" versions).
- **List import** of `Artist - Title` lines: `/proxy/import`.

## Run

```bash
docker compose up -d --build
```

Main settings: `NAVIDROME_URL`, `MUSIC_DIR` (the folder Navidrome scans), `DATA_DIR`,
`SLSKD_URL` / `SLSKD_API_KEY`, `BACKGROUND_WORKERS` (5). All options:
`src/navidrome_proxy/config.py`.

Migrating from the first proxy:

```bash
docker compose run --rm navidrome-proxy migrate-legacy --state /data/legacy-state.json
docker compose run --rm navidrome-proxy remux-dash --dry-run   # fix DASH-fragmented files
docker compose run --rm navidrome-proxy relayout --dry-run     # Artist/Album folder layout
```

## Tests

```bash
uv run pytest                  # unit and end-to-end
uv run pytest -m integration   # real Navidrome in Docker
uv run pytest -m network       # real YouTube Music / SoundCloud
uv run python tools/arpeggi_emulator.py --url http://host:8084 --user U --password P --query "..."
```

`tools/avplayer` checks playback with AVQueuePlayer, the engine Arpeggi uses.

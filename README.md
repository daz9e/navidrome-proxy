<p align="center">
  <img src="docs/banner.png" alt="navidrome-proxy" width="100%">
</p>

<p align="center">
  An OpenSubsonic proxy that puts the whole music catalog inside your
  <a href="https://www.navidrome.org/">Navidrome</a> library.<br>
  Search anything, press play, and the track becomes yours.
</p>

<p align="center">
  <a href="https://github.com/daz9e/navidrome-proxy/actions/workflows/ci.yml"><img src="https://github.com/daz9e/navidrome-proxy/actions/workflows/ci.yml/badge.svg" alt="ci"></a>
</p>

---

Point any Subsonic client (Arpeggi, Substreamer, Symfonium, the Navidrome web UI) at the proxy
instead of Navidrome. Nothing else changes for the client.

| You do | The proxy does |
|---|---|
| **Search** | Your library first, then Deezer and YouTube Music, deduplicated. Nothing is downloaded. |
| **Play** | Downloads the track in a priority lane (~4 s to first sound), tags it and streams it; prefetches the next tracks of the album. |
| **Like / add to playlist** | Downloads it, then sets the like or playlist entry in Navidrome itself. |
| **Open an album** | Shows downloaded and not-yet-downloaded tracks together, in order. |

**One song, one id.** Once a track is in the library, clients see the real Navidrome song
everywhere. Deezer, YouTube and SoundCloud copies of the same recording are merged.

**A clean library.** Files are remuxed (no broken DASH durations), tagged from Deezer
(all artists of a collaboration), and stored as `Artist/Album/01-03 - Title [deezer-id].m4a`.

**Better copies of liked tracks.** A background job looks on Soulseek and replaces the file
only if its audio fingerprint matches, so an instrumental never slips in.

**List import.** Paste `Artist - Title` lines at `/proxy/import`: they are matched, liked and
downloaded, optionally into a playlist in the original order.

## Run

```bash
docker compose up -d --build
```

| Variable | Default | |
|---|---|---|
| `NAVIDROME_URL` | `http://127.0.0.1:4533` | Navidrome address |
| `MUSIC_DIR` | `/music` | the folder Navidrome scans |
| `DATA_DIR` | `/data` | SQLite state and cover cache |
| `SLSKD_URL`, `SLSKD_API_KEY` | | enables Soulseek upgrades |
| `BACKGROUND_WORKERS` | `5` | parallel background downloads |

All options: [`config.py`](src/navidrome_proxy/config.py).
Moving from the first version: `migrate-legacy`, `adopt-youtube-fallbacks`, `remux-dash`,
`repair-covers`, then `relayout` once Navidrome has rescanned (all but the first support `--dry-run`).
Stop the server for these (only `remux-dash` runs next to it); verified database backups go to
`DATA_DIR/backups` every 6 hours.

## Develop

```bash
uv sync
uv run pytest                   # unit + end-to-end
uv run pytest -m integration    # real Navidrome in Docker
```

`tools/arpeggi_emulator.py` replays a real Arpeggi session against a server;
`tools/avplayer` checks playback with AVQueuePlayer, the engine Arpeggi uses.

"""Command line entry point: ``navidrome-proxy [serve]``."""

from __future__ import annotations

import argparse
import asyncio
import json
import logging
import sys
from pathlib import Path

from aiohttp import web

from .config import Settings


def configure_logging(level: str) -> None:
    logging.basicConfig(
        level=getattr(logging, level, logging.INFO),
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        stream=sys.stdout,
    )
    logging.getLogger("aiohttp.access").setLevel(logging.WARNING)


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(prog="navidrome-proxy")
    sub = parser.add_subparsers(dest="command")
    sub.add_parser("serve", help="run the proxy (default)")
    legacy = sub.add_parser("migrate-legacy", help="adopt the first proxy's files and state.json")
    legacy.add_argument("--state", type=Path, help="legacy state.json")
    legacy.add_argument(
        "--legacy-music-root", default="/music", help="music path used inside the old container"
    )
    layout = sub.add_parser("relayout", help="move files into the Artist/Album layout")
    layout.add_argument("--dry-run", action="store_true")
    sub.add_parser("check-db", help="full integrity check of the state database")
    remux = sub.add_parser("remux-dash", help="losslessly fix DASH-fragmented m4a files")
    remux.add_argument("--dry-run", action="store_true")
    args = parser.parse_args(argv)
    settings = Settings.from_env()
    configure_logging(settings.log_level)
    if args.command in (None, "serve"):
        from .app import create_app

        return web.run_app(
            create_app(settings),
            host=settings.host,
            port=settings.port,
            access_log=None,
            print=lambda *_: logging.getLogger(__name__).info(
                "listening on %s:%d", settings.host, settings.port
            ),
        )

    from . import migrate
    from .db import Store

    store = Store(settings.db_path)
    try:
        if args.command == "migrate-legacy":
            result = asyncio.run(
                migrate.migrate_legacy(store, settings.music_dir, args.state, args.legacy_music_root)
            )
        elif args.command == "check-db":
            problems = store.integrity(quick=False)
            result = {"ok": not problems, "problems": problems[:20]}
        elif args.command == "relayout":
            result = migrate.relayout(store, settings.music_dir, dry_run=args.dry_run)
        else:
            result = asyncio.run(migrate.remux_dash(settings.music_dir, dry_run=args.dry_run))
    finally:
        store.close()
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()

"""Command line entry point: ``navidrome-proxy [serve]``."""

from __future__ import annotations

import argparse
import logging
import sys

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
    args = parser.parse_args(argv)
    settings = Settings.from_env()
    configure_logging(settings.log_level)
    if args.command in (None, "serve"):
        from .app import create_app

        web.run_app(
            create_app(settings),
            host=settings.host,
            port=settings.port,
            access_log=None,
            print=lambda *_: logging.getLogger(__name__).info(
                "listening on %s:%d", settings.host, settings.port
            ),
        )


if __name__ == "__main__":
    main()

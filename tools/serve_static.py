"""Serve a folder with HTTP Range support (like the proxy does), for player tests."""

import sys

from aiohttp import web

root = sys.argv[1]
port = int(sys.argv[2]) if len(sys.argv) > 2 else 18090
app = web.Application()
app.router.add_static("/", root)
web.run_app(app, host="127.0.0.1", port=port, print=None)

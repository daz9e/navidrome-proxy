#!/bin/sh
# YouTube changes often; a stale yt-dlp is the most common cause of 403s and
# missing formats. Refresh it on every start (best effort, never fatal).
if [ "$1" = "serve" ] && [ "${YTDLP_AUTO_UPDATE:-1}" != "0" ]; then
    timeout 90 pip install --no-cache-dir --quiet --disable-pip-version-check --upgrade --pre \
        "yt-dlp[default,deno]" >/dev/null 2>&1 \
        && echo "yt-dlp $(python -c 'import yt_dlp; print(yt_dlp.version.__version__)')" \
        || echo "yt-dlp update skipped"
fi
exec navidrome-proxy "$@"

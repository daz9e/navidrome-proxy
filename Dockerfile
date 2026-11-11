FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --locked --release

FROM python:3.13-slim-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg libchromaprint-tools ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# yt-dlp (with its deno runtime) is the only Python dependency of the runtime.
RUN pip install --no-cache-dir "yt-dlp[default,deno]"

COPY --from=build /src/target/release/navidrome-proxy /usr/local/bin/navidrome-proxy
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

ENV DATA_DIR=/data \
    MUSIC_DIR=/music \
    PORT=8084 \
    YTDLP_AUTO_UPDATE=1

EXPOSE 8084
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s \
    CMD python -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8084/proxy/v1/health', timeout=4)"
ENTRYPOINT ["/entrypoint.sh"]
CMD ["serve"]

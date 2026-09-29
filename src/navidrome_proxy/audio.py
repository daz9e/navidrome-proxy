"""Audio container handling with ffmpeg/ffprobe.

YouTube delivers AAC as a *fragmented DASH* MP4 (``major_brand=dash``). Without a
remux, AVFoundation (Arpeggi's AVQueuePlayer) reads such a file with a wrong,
roughly doubled duration: the progress bar runs past the real end into negative
time and the queue never advances to the next song. Every downloaded file is
therefore rewritten into a plain MP4 with the index at the front (``faststart``),
which also makes the first byte playable while the rest is still transferring.
"""

from __future__ import annotations

import asyncio
import json
import logging
import shutil
from dataclasses import asdict, dataclass
from pathlib import Path

log = logging.getLogger(__name__)

AUDIO_SUFFIXES = {".m4a", ".mp4", ".mp3", ".opus", ".ogg", ".oga", ".webm", ".flac", ".wav", ".aac", ".mka"}


class AudioError(Exception):
    pass


@dataclass
class AudioInfo:
    duration: float
    codec: str
    bitrate: int  # kbit/s
    sample_rate: int
    channels: int
    size: int
    suffix: str
    brand: str = ""

    def as_dict(self) -> dict:
        return asdict(self)


async def _run(*args: str, timeout: float = 300.0) -> tuple[int, bytes, bytes]:
    proc = await asyncio.create_subprocess_exec(
        *args, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
    )
    try:
        out, err = await asyncio.wait_for(proc.communicate(), timeout)
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise AudioError(f"{args[0]} timed out") from None
    return proc.returncode or 0, out, err


def tools_available() -> dict[str, bool]:
    return {name: shutil.which(name) is not None for name in ("ffmpeg", "ffprobe", "fpcalc")}


async def probe(path: Path) -> AudioInfo:
    code, out, err = await _run(
        "ffprobe",
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        "-select_streams",
        "a:0",
        str(path),
        timeout=60,
    )
    if code != 0:
        raise AudioError(f"ffprobe failed: {err.decode(errors='replace')[-300:]}")
    data = json.loads(out or b"{}")
    streams = data.get("streams") or []
    if not streams:
        raise AudioError("no audio stream")
    stream, fmt = streams[0], data.get("format") or {}
    duration = float(stream.get("duration") or fmt.get("duration") or 0)
    bitrate = int(stream.get("bit_rate") or fmt.get("bit_rate") or 0) // 1000
    return AudioInfo(
        duration=duration,
        codec=str(stream.get("codec_name") or ""),
        bitrate=bitrate,
        sample_rate=int(stream.get("sample_rate") or 0),
        channels=int(stream.get("channels") or 0),
        size=path.stat().st_size,
        suffix=path.suffix.lstrip(".").lower(),
        brand=str((fmt.get("tags") or {}).get("major_brand") or ""),
    )


def target_suffix(codec: str) -> str:
    """Container the library stores a codec in; everything exotic becomes AAC/m4a."""
    return "mp3" if codec == "mp3" else "m4a"


async def normalize(src: Path, dest_dir: Path, stem: str = "audio") -> tuple[Path, AudioInfo]:
    """Produce a clean, player-friendly file from any downloaded source.

    * AAC  -> stream copy into a plain MP4 with ``+faststart`` (fixes DASH files)
    * MP3  -> stream copy, all foreign tags dropped
    * else -> transcode to AAC 192 kbit/s in MP4 (Opus/Vorbis/WebM are not
      reliably playable by AVFoundation over HTTP)
    Metadata is always dropped; canonical tags are written afterwards.
    """
    info = await probe(src)
    if info.duration < 20:
        raise AudioError(f"audio too short ({info.duration:.1f}s)")
    suffix = target_suffix(info.codec)
    dest = dest_dir / f"{stem}.{suffix}"
    args = [
        "ffmpeg",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-i",
        str(src),
        "-map",
        "0:a:0",
        "-map_metadata",
        "-1",
        "-vn",
        "-sn",
        "-dn",
    ]
    if info.codec == "aac":
        args += ["-c:a", "copy", "-movflags", "+faststart", "-f", "mp4"]
    elif info.codec == "mp3":
        args += ["-c:a", "copy", "-id3v2_version", "3", "-f", "mp3"]
    else:
        args += ["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart", "-f", "mp4"]
    args.append(str(dest))
    code, _, err = await _run(*args, timeout=600)
    if code != 0 or not dest.is_file():
        raise AudioError(f"ffmpeg failed: {err.decode(errors='replace')[-300:]}")
    out = await probe(dest)
    if info.duration and abs(out.duration - info.duration) > 2:
        raise AudioError(f"duration changed while remuxing: {info.duration:.1f}s -> {out.duration:.1f}s")
    return dest, out


async def remux_in_place(path: Path) -> bool:
    """Rewrite a DASH-fragmented MP4 in place (lossless). Returns True if changed.

    Existing tags are preserved (``-map_metadata 0``), unlike :func:`normalize`.
    """
    info = await probe(path)
    if info.codec != "aac" or info.brand != "dash":
        return False
    tmp = path.with_name(f".{path.stem}.remux{path.suffix}")
    code, _, err = await _run(
        "ffmpeg",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-i",
        str(path),
        "-map",
        "0",
        "-map_metadata",
        "0",
        "-c",
        "copy",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
        str(tmp),
        timeout=600,
    )
    if code != 0 or not tmp.is_file():
        tmp.unlink(missing_ok=True)
        raise AudioError(f"remux failed for {path.name}: {err.decode(errors='replace')[-300:]}")
    fixed = await probe(tmp)
    if abs(fixed.duration - info.duration) > 2 and fixed.duration > 0:
        # The DASH header lies about duration; trust the rewritten file only if sane.
        log.info("remux %s: header duration %.1fs -> real %.1fs", path.name, info.duration, fixed.duration)
    tmp.replace(path)
    return True


async def fingerprint(path: Path, length: int = 120) -> list[int] | None:
    """Raw Chromaprint fingerprint (needs ``fpcalc``), or None if unavailable."""
    if shutil.which("fpcalc") is None:
        return None
    code, out, _ = await _run("fpcalc", "-raw", "-json", "-length", str(length), str(path), timeout=120)
    if code != 0:
        return None
    try:
        return [int(x) for x in json.loads(out)["fingerprint"]]
    except (ValueError, KeyError):
        return None


def fingerprint_similarity(a: list[int], b: list[int], max_offset: int = 80) -> float:
    """Best bitwise similarity (0..1) of two raw fingerprints over small time offsets."""
    if not a or not b:
        return 0.0
    best = 0.0
    for offset in range(-max_offset, max_offset + 1):
        pairs = [(a[i], b[i + offset]) for i in range(len(a)) if 0 <= i + offset < len(b)]
        if len(pairs) < 30:
            continue
        diff = sum(bin((x ^ y) & 0xFFFFFFFF).count("1") for x, y in pairs)
        best = max(best, 1 - diff / (32 * len(pairs)))
    return best

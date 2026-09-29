"""Synthetic audio files made with ffmpeg (no copyrighted material in the repo)."""

import subprocess
from pathlib import Path


def make(path: Path, seconds: int = 30, codec: str = "aac", dash: bool = False, freq: int = 440) -> Path:
    args = [
        "ffmpeg",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        f"sine=frequency={freq}:duration={seconds}",
    ]
    if codec == "aac":
        args += ["-c:a", "aac", "-b:a", "128k"]
        if dash:
            args += ["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-brand", "dash", "-f", "mp4"]
    elif codec == "mp3":
        args += [
            "-c:a",
            "libmp3lame",
            "-b:a",
            "192k",
            "-metadata",
            "title=junk",
            "-metadata",
            "comment=youtube",
        ]
    elif codec == "opus":
        args += ["-c:a", "libopus", "-b:a", "96k", "-f", "webm"]
    args.append(str(path))
    subprocess.run(args, check=True)
    return path


def make_music(path: Path, seconds: int = 40, variant: int = 0) -> Path:
    """A melody-like signal with changing notes, so fingerprints carry information."""
    notes = (
        [220, 262, 294, 330, 349, 392, 440, 494] if variant == 0 else [247, 311, 370, 415, 466, 523, 587, 622]
    )
    expr = "+".join(
        f"0.3*sin(2*PI*{f}*t)*between(mod(t\\,{len(notes)})\\,{i}\\,{i + 1})" for i, f in enumerate(notes)
    )
    subprocess.run(
        [
            "ffmpeg",
            "-nostdin",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            f"aevalsrc={expr}:s=44100:d={seconds}",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            str(path),
        ],
        check=True,
    )
    return path

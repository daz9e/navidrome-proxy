import shutil

import pytest

from navidrome_proxy import audio

from .audio_fixtures import make, make_music

pytestmark = pytest.mark.skipif(shutil.which("ffmpeg") is None, reason="ffmpeg missing")


async def test_dash_aac_is_remuxed_to_plain_faststart_mp4(tmp_path):
    src = make(tmp_path / "source.m4a", 30, dash=True)
    assert (await audio.probe(src)).brand == "dash"
    out, info = await _normalize(src, tmp_path)
    assert out.suffix == ".m4a"
    assert info.codec == "aac" and info.brand != "dash"
    assert abs(info.duration - 30) < 0.5
    data = out.read_bytes()
    assert data.find(b"moov") < data.find(b"mdat")  # index before payload (faststart)


async def _normalize(src, tmp_path):
    dest = tmp_path / "out"
    dest.mkdir()
    return await audio.normalize(src, dest)


async def test_opus_is_transcoded_to_aac(tmp_path):
    src = make(tmp_path / "source.webm", 25, codec="opus")
    out, info = await _normalize(src, tmp_path)
    assert out.suffix == ".m4a" and info.codec == "aac"


async def test_mp3_is_copied_without_foreign_tags(tmp_path):
    src = make(tmp_path / "source.mp3", 25, codec="mp3")
    out, info = await _normalize(src, tmp_path)
    assert out.suffix == ".mp3" and info.codec == "mp3"
    import mutagen

    assert "TIT2" not in (mutagen.File(out).tags or {})


async def test_too_short_audio_is_rejected(tmp_path):
    src = make(tmp_path / "source.m4a", 5)
    with pytest.raises(audio.AudioError):
        await _normalize(src, tmp_path)


async def test_remux_in_place_only_touches_dash(tmp_path):
    dash = make(tmp_path / "a.m4a", 30, dash=True)
    plain = make(tmp_path / "b.m4a", 30)
    assert await audio.remux_in_place(dash)
    assert (await audio.probe(dash)).brand != "dash"
    assert not await audio.remux_in_place(plain)


@pytest.mark.skipif(shutil.which("fpcalc") is None, reason="fpcalc missing")
async def test_fingerprint_distinguishes_recordings(tmp_path):
    a = make_music(tmp_path / "a.m4a", variant=0)
    a2 = tmp_path / "a2.m4a"
    shutil.copy(a, a2)
    b = make_music(tmp_path / "b.m4a", variant=1)
    fa, fa2, fb = [await audio.fingerprint(p) for p in (a, a2, b)]
    assert audio.fingerprint_similarity(fa, fa2) > 0.95
    assert audio.fingerprint_similarity(fa, fb) < audio.fingerprint_similarity(fa, fa2)

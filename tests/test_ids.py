import pytest

from navidrome_proxy import ids


@pytest.mark.parametrize(
    ("raw", "kind", "key"),
    [
        ("deezer:136889400", "track", "136889400"),
        ("deezer-album:14652356", "album", "14652356"),
        ("deezer-artist:4050205", "artist", "4050205"),
        ("yt:sO4pSj466Hk", "youtube", "sO4pSj466Hk"),
    ],
)
def test_parse_round_trips(raw, kind, key):
    vid = ids.parse(raw)
    assert vid is not None
    assert (vid.kind, vid.key) == (kind, key)
    assert str(vid) == raw


@pytest.mark.parametrize(
    "raw",
    [
        "",
        None,
        "kTKA4zXbiyUWyEGsxfk9ud",
        "al-7w4jZDKHpAPFzdeYIwvR4j_6aadb1f4",
        "deezer:",
        "deezer:12a",
        "yt:short",
        "deezer-album:../../etc",
    ],
)
def test_navidrome_and_malformed_ids_are_not_virtual(raw):
    assert ids.parse(raw) is None
    assert not ids.is_virtual(raw)


def test_track_kinds():
    assert ids.parse("deezer:1").is_track
    assert ids.parse("yt:sO4pSj466Hk").is_track
    assert not ids.parse("deezer-album:1").is_track

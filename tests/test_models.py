import json
from pathlib import Path

from navidrome_proxy.subsonic import models

REFERENCE = json.loads((Path(__file__).parent / "fixtures" / "navidrome_0.63.json").read_text())

META = {
    "title": "Hella Quality",
    "artist": "TVETH, Bato",
    "artists": ["TVETH", "Bato"],
    "artist_id": "1",
    "album": "RAW",
    "album_id": "100",
    "album_artist": "TVETH",
    "duration": 214,
    "track": 3,
    "disc": 1,
    "year": 2026,
    "genre": "Rap",
    "isrc": "ISRC1",
    "explicit": True,
}
DEEZER_ALBUM = {
    "id": 100,
    "title": "RAW",
    "release_date": "2026-08-28",
    "nb_tracks": 12,
    "record_type": "album",
    "genres": {"data": [{"name": "Rap"}]},
    "artist": {"id": 1, "name": "TVETH"},
    "label": "Label",
}


def shape(value):
    """JSON type signature, recursing into objects (lists by their first element)."""
    if isinstance(value, dict):
        return {k: shape(v) for k, v in value.items()}
    if isinstance(value, list):
        return [shape(value[0])] if value else []
    return type(value).__name__


def assert_same_keys_and_types(virtual, reference, optional=()):
    missing = set(reference) - set(virtual) - set(optional)
    assert not missing, f"missing keys: {sorted(missing)}"
    for key, ref in reference.items():
        if key not in virtual:
            continue
        got = virtual[key]
        assert type(got) is type(ref), f"{key}: {type(got).__name__} != {type(ref).__name__}"
        if isinstance(ref, list) and ref and got:
            assert set(shape(got[0])) == set(shape(ref[0])) if isinstance(ref[0], dict) else True, key
    assert None not in virtual.values()


def test_virtual_song_matches_navidrome_song():
    song = models.song(
        "deezer:4250555171",
        META,
        audio={"suffix": "m4a", "size": 3_000_000, "bitrate": 128, "duration": 213.9},
    )
    assert_same_keys_and_types(song, REFERENCE["song"])
    assert song["duration"] == 214
    assert song["artists"] == [{"id": "deezer-artist:1", "name": "TVETH"}, {"id": "", "name": "Bato"}]
    assert song["albumId"] == song["parent"] == song["coverArt"] == "deezer-album:100"
    assert song["explicitStatus"] == "explicit"


def test_virtual_song_without_album_or_audio():
    song = models.song("yt:sO4pSj466Hk", {"title": "Кошмар", "artist": "ЮПИ", "duration": 150})
    assert_same_keys_and_types(song, REFERENCE["song"])
    assert song["coverArt"] == "yt:sO4pSj466Hk"
    assert song["albumId"] == ""
    assert song["discNumber"] == 1


def test_starred_is_an_iso_string_never_a_bool():
    song = models.song("deezer:1", META, starred=1_700_000_000)
    assert song["starred"] == "2023-11-14T22:13:20Z"


def test_virtual_album_matches_navidrome_album():
    songs = [models.song("deezer:1", META)]
    album = models.album("deezer-album:100", DEEZER_ALBUM, songs=songs)
    reference = REFERENCE["album"]
    assert_same_keys_and_types({k: v for k, v in album.items() if k != "song"}, reference)
    assert album["songCount"] == 1
    assert album["releaseDate"] == {"year": 2026, "month": 8, "day": 28}
    assert album["duration"] == 214
    listed = models.album("deezer-album:100", DEEZER_ALBUM)
    assert listed["songCount"] == 12 and "song" not in listed


def test_virtual_artist_matches_navidrome_artist():
    artist = models.artist("deezer-artist:1", {"name": "TVETH", "nb_album": 3, "picture_xl": "https://x"})
    assert_same_keys_and_types(artist, REFERENCE["artist"])
    assert artist["albumCount"] == 3


def test_iso_accepts_dates_and_garbage():
    assert models.iso("2020-05-01") == "2020-05-01T00:00:00Z"
    assert models.iso("2026-09-14T10:12:39.462723Z") == "2026-09-14T10:12:39Z"
    assert models.iso("not a date").endswith("Z")

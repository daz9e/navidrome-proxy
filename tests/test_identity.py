import pytest

from navidrome_proxy.db import Store
from navidrome_proxy.identity import Identity, recording_key


@pytest.fixture
def world(tmp_path):
    store = Store(tmp_path / "db.sqlite3")
    music = tmp_path / "music"
    music.mkdir()
    yield store, Identity(store), music
    store.close()


def ready(store, music, track_id, meta, source=None, ref=None, duration=None):
    f = music / f"{track_id.replace(':', '-')}.m4a"
    f.write_bytes(b"x")
    store.remember_track(track_id, meta)
    store.update_track(
        track_id,
        status="ready",
        file=str(f),
        source=source,
        source_ref=ref,
        audio={"duration": duration or meta.get("duration", 0)},
    )
    return store.get_track(track_id)


def test_recording_key_ignores_feat_brackets_and_collab_order():
    assert recording_key(["Юпи"], "Порезы на руках") == "юпи|порезы на руках"
    assert recording_key("TVETH, Bato", "Hella Quality (feat. Bato)") == "tveth|hella quality"
    assert recording_key([], "x") is None


def test_download_source_becomes_alias(world):
    store, identity, music = world
    t = ready(
        store,
        music,
        "deezer:4274804272",
        {"title": "Порезы на руках", "artists": ["Юпи"], "duration": 124, "isrc": "ru123"},
        source="youtube",
        ref="Q1QvyIrUzz0",
    )
    identity.register(t)
    assert identity.canonical("yt:Q1QvyIrUzz0") == "deezer:4274804272"
    assert identity.canonical("deezer:4274804272") == "deezer:4274804272"


def test_same_song_found_under_other_catalog_and_title(world):
    store, identity, music = world
    sc = ready(
        store,
        music,
        "sc:2398309779",
        {"title": "Порезы на руках", "artists": ["Юпи"], "duration": 122},
        source="soundcloud",
        ref="2398309779",
    )
    identity.register(sc)
    # Deezer later lists the song with a feat credit in the title and 2 s longer.
    found = identity.find_existing(
        {"title": "Порезы на руках (feat. X)", "artists": ["Юпи"], "duration": 124}
    )
    assert found is not None and found.id == "sc:2398309779"
    # A different recording (sped up edit, much shorter) is not merged.
    assert identity.find_existing({"title": "Порезы на руках", "artists": ["Юпи"], "duration": 90}) is None


def test_isrc_match_wins_over_titles(world):
    store, identity, music = world
    t = ready(store, music, "deezer:1", {"title": "Song", "artists": ["A"], "duration": 200, "isrc": "US1"})
    identity.register(t)
    assert (
        identity.find_existing({"title": "Totally Different", "artists": ["B"], "isrc": "us1"}).id
        == "deezer:1"
    )


def test_link_follows_chains_and_refuses_cycles(world):
    _, identity, _ = world
    assert identity.link("yt:AAAAAAAAAAA", "deezer:1", "manual")
    assert identity.link("deezer:2", "yt:AAAAAAAAAAA", "reconciler")
    assert identity.canonical("deezer:2") == "deezer:1"
    assert not identity.link("deezer:1", "deezer:2", "cycle")
    assert identity.canonical_many(["deezer:2", "deezer:9"]) == {
        "deezer:2": "deezer:1",
        "deezer:9": "deezer:9",
    }


def test_not_ready_tracks_are_not_reused(world):
    store, identity, _ = world
    store.remember_track("deezer:1", {"title": "Song", "artists": ["A"], "duration": 200})
    store.add_recording("a|song", "deezer:1", 200)
    assert identity.find_existing({"title": "Song", "artists": ["A"], "duration": 200}) is None
    assert (
        identity.find_existing({"title": "Song", "artists": ["A"], "duration": 200}, usable=False).id
        == "deezer:1"
    )

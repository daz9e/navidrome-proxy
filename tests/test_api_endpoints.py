"""Endpoints beyond the main flows: album/artist likes, ratings, play queues, catalog info."""

from .conftest import candidate
from .fakes import song
from .test_app import until


async def _download(world, track_id="deezer:1001", title="Hella Quality", ref="dEvrb6N70kA"):
    world.finder.results[title] = [candidate(ref, title)]
    await (await world.get("stream", f"id={track_id}")).read()
    store = world.services.store
    await until(lambda: store.get_track(track_id).local_id)
    return store.get_track(track_id).local_id


# ---------------------------------------------------------------------- likes
async def test_album_like_queues_every_track_and_shows_as_starred(world):
    assert (await world.json("star", "albumId=deezer-album:100"))["status"] == "ok"
    store = world.services.store
    jobs = [store.get_job(f"deezer:{n}", "download") for n in (1001, 1002, 1003)]
    assert all(j is not None and j.priority == 2 and j.reason == "like-album:alice" for j in jobs)
    starred = (await world.json("getStarred2"))["starred2"]
    assert [a["id"] for a in starred["album"]] == ["deezer-album:100"]

    await world.json("unstar", "albumId=deezer-album:100")
    assert store.pending_stars("alice") == []


async def test_artist_like_is_kept_by_the_proxy(world):
    await world.json("star", "artistId=deezer-artist:1")
    starred = (await world.json("getStarred2"))["starred2"]
    assert [a["id"] for a in starred["artist"]] == ["deezer-artist:1"]
    assert starred["artist"][0]["starred"]
    await world.json("unstar", "artistId=deezer-artist:1")
    assert world.services.store.pending_stars("alice") == []


async def test_xml_get_starred_is_passed_through(world):
    await world.json("star", "artistId=deezer-artist:1")
    resp = await world.get("getStarred2", "f=xml", auth="u=alice&p=alicepw&v=1.16.1&c=t")
    assert resp.status == 200
    assert "deezer-artist" not in await resp.text()


async def test_rating_of_a_track_that_is_not_downloaded_is_accepted_and_dropped(world):
    assert (await world.json("setRating", "id=deezer:1003&rating=5"))["status"] == "ok"
    assert not any(m == "setRating" for m, _ in world.navidrome.requests)


async def test_rating_of_a_downloaded_track_goes_to_its_navidrome_song(world):
    local_id = await _download(world)
    await world.json("setRating", "id=deezer:1001&rating=4")
    sent = [q for m, q in world.navidrome.requests if m == "setRating"]
    assert sent and sent[-1]["id"] == local_id and sent[-1]["rating"] == "4"


# ------------------------------------------------------------------ play queue
def _last(world, method):
    return [q for m, q in world.navidrome.repeated if m == method][-1]


async def test_play_queue_keeps_only_tracks_navidrome_knows(world):
    local_id = await _download(world)
    world.navidrome.songs["L1"] = song("L1", "Local", "X")
    await world.json("savePlayQueue", "id=L1&id=deezer:1001&id=deezer:1003&current=deezer:1001&position=5")
    sent = _last(world, "savePlayQueue")
    assert sent["id"] == ["L1", local_id] and sent["current"] == [local_id] and sent["position"] == ["5"]

    # The current track is not local yet: no current, no position.
    await world.json("savePlayQueue", "id=L1&id=deezer:1003&current=deezer:1003&position=5")
    sent = _last(world, "savePlayQueue")
    assert sent["id"] == ["L1"] and "current" not in sent and "position" not in sent

    calls = len(world.navidrome.repeated)
    root = await world.json("savePlayQueue", "id=deezer:1003&current=deezer:1003")
    assert root["status"] == "ok" and len(world.navidrome.repeated) == calls  # nothing local: not sent


async def test_play_queue_by_index_moves_the_index_with_dropped_tracks(world):
    local_id = await _download(world)
    world.navidrome.songs["L1"] = song("L1", "Local", "X")
    await world.json("savePlayQueueByIndex", "id=deezer:1003&id=L1&id=deezer:1001&currentIndex=2")
    sent = _last(world, "savePlayQueueByIndex")
    assert sent["id"] == ["L1", local_id] and sent["currentIndex"] == ["1"]


# ------------------------------------------------------------------- playback
async def test_transcoding_request_for_a_downloaded_track_goes_to_navidrome(world):
    local_id = await _download(world)
    world.navidrome.songs[local_id] = world.navidrome.songs.get(local_id) or song(local_id, "t", "a")
    resp = await world.get("stream", "id=deezer:1001&format=mp3")
    assert resp.status == 200
    streamed = [q for m, q in world.navidrome.requests if m == "stream"]
    assert streamed and streamed[-1]["id"] == local_id and streamed[-1]["format"] == "mp3"


async def test_raw_stream_of_a_downloaded_track_is_served_by_the_proxy(world):
    await _download(world)
    before = len([m for m, _ in world.navidrome.requests if m == "stream"])
    resp = await world.get("stream", "id=deezer:1001&format=raw")
    assert resp.status == 200 and resp.headers["Content-Type"] == "audio/mp4"
    assert len([m for m, _ in world.navidrome.requests if m == "stream"]) == before


async def test_download_endpoint_names_the_file(world):
    await _download(world)
    resp = await world.get("download", "id=deezer:1001")
    assert resp.status == 200
    assert resp.headers["Content-Disposition"].startswith("attachment; filename*=UTF-8''")


async def test_stream_of_an_album_id_is_passed_through(world):
    resp = await world.get("stream", "id=deezer-album:100")
    assert resp.status == 200  # not a track: Navidrome answers (here: song not found)


# --------------------------------------------------------------------- catalog
async def test_top_songs_come_from_the_catalog_when_navidrome_has_none(world):
    root = await world.json("getTopSongs", "artist=TVETH&count=2")
    songs = root["topSongs"]["song"]
    assert [x["id"] for x in songs] == ["deezer:1001", "deezer:1002"]


async def test_similar_songs_of_a_catalog_artist(world):
    root = await world.json("getSimilarSongs2", "id=deezer-artist:1")
    assert {x["id"] for x in root["similarSongs2"]["song"]} >= {"deezer:1001"}
    other = await world.json("getSimilarSongs2", "id=deezer:1001")
    assert other["similarSongs2"] == {}


async def test_album_info_and_wrong_kinds_are_empty_not_errors(world):
    info = (await world.json("getAlbumInfo2", "id=deezer-album:100"))["albumInfo"]
    assert info["largeImageUrl"].startswith("https://")
    assert (await world.json("getAlbumInfo2", "id=deezer-artist:1"))["albumInfo"] == {}
    assert (await world.json("getArtistInfo", "id=deezer-album:100"))["artistInfo"] == {}


async def test_unknown_catalog_ids_are_not_found(world):
    root = await world.json("getArtist", "id=deezer-album:100")
    assert root["status"] == "failed" and root["error"]["code"] == 70
    root = await world.json("getArtist", "id=deezer-artist:999")
    assert root["status"] == "failed"


async def test_lyrics_of_an_undownloaded_track_are_empty(world):
    root = await world.json("getLyricsBySongId", "id=deezer:1003")
    assert root["lyricsList"] == {}

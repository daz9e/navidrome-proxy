"""End-to-end behaviour of the proxy against fake upstreams (real ffmpeg, tags and files)."""

import asyncio

from .conftest import BOB, candidate
from .fakes import song


async def until(predicate, timeout=10.0):
    for _ in range(int(timeout / 0.02)):
        result = predicate()
        if asyncio.iscoroutine(result):
            result = await result
        if result:
            return result
        await asyncio.sleep(0.02)
    raise AssertionError("condition not reached")


# ------------------------------------------------------------------ plumbing
async def test_unknown_endpoints_and_non_api_paths_pass_through(world):
    root = await world.json("getMusicFolders")
    assert root["echo"]["method"] == "getMusicFolders"
    ui = await world.client.get("/app/")
    assert await ui.text() == "navidrome ui /app/"


async def test_wrong_credentials_on_proxy_endpoint_is_a_subsonic_error(world):
    root = await world.json("search3", "query=hella", auth="u=alice&p=wrong&f=json")
    assert root["status"] == "failed" and root["error"]["code"] == 40


async def test_form_post_parameters_are_supported(world):
    resp = await world.client.post(
        "/rest/getSong.view",
        data="u=alice&p=alicepw&f=json&id=deezer:1001",
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    assert (await resp.json())["subsonic-response"]["song"]["title"] == "Hella Quality"


async def test_health(world):
    assert (await (await world.client.get("/proxy/v1/health")).json()) == {"ok": True}


# -------------------------------------------------------------------- search
async def test_search_merges_local_catalog_and_youtube(world):
    world.navidrome.songs["L1"] = song("L1", "Hella Quality Live Bootleg", "TVETH")
    world.finder.ytmusic_rows = [
        {
            "video_id": "AAAAAAAAAAA",
            "title": "Hella Quality",
            "artists": ["TVETH"],
            "duration": 30,
            "album": "RAW",
            "thumbnail": None,
            "explicit": False,
        },  # same as the Deezer hit: dropped
        {
            "video_id": "BBBBBBBBBBB",
            "title": "Hella Quality Pt. 2",
            "artists": ["TVETH"],
            "duration": 40,
            "album": None,
            "thumbnail": None,
            "explicit": False,
        },
    ]
    root = await world.json("search3", "query=hella%20quality&songCount=50&albumCount=5&artistCount=5")
    songs = root["searchResult3"]["song"]
    assert [x["id"] for x in songs] == ["L1", "deezer:1001", "yt:BBBBBBBBBBB"]
    assert songs[1]["artists"] == [{"id": "deezer-artist:1", "name": "TVETH"}]


async def test_search_returns_catalog_albums_and_artists(world):
    root = await world.json("search3", "query=tveth&songCount=0&albumCount=5&artistCount=5")
    assert [a["id"] for a in root["searchResult3"]["album"]] == ["deezer-album:100"]
    assert [a["id"] for a in root["searchResult3"]["artist"]] == ["deezer-artist:1"]


async def test_search_drops_external_twins_of_local_songs_with_other_spelling(world):
    world.navidrome.songs["L1"] = song("L1", "Hella Quality", "ТВЕТ", duration=30)
    world.finder.ytmusic_rows = [
        {
            "video_id": "AAAAAAAAAAA",
            "title": "Hella Quality",
            "artists": ["TVETH"],
            "duration": 31,
            "album": None,
            "thumbnail": None,
            "explicit": False,
        }
    ]
    root = await world.json("search3", "query=hella%20quality&songCount=50&albumCount=0&artistCount=0")
    assert [x["id"] for x in root["searchResult3"]["song"]] == ["L1"]


async def test_search_pagination_ends_instead_of_repeating_external_results(world):
    for i in range(3):
        world.navidrome.songs[f"L{i}"] = song(f"L{i}", f"Hella Quality {i}", "Local")
    first = await world.json("search3", "query=hella&songCount=2000&songOffset=0&albumCount=0&artistCount=0")
    got = first["searchResult3"]["song"]
    assert len(got) == 4  # 3 local + 1 external
    second = await world.json(
        "search3", f"query=hella&songCount=2000&songOffset={len(got)}&albumCount=0&artistCount=0"
    )
    assert second["searchResult3"].get("song", []) == []


async def test_search_pagination_continues_local_results(world):
    for i in range(5):
        world.navidrome.songs[f"L{i}"] = song(f"L{i}", f"Hella Quality {i}", "Local")
    first = await world.json("search3", "query=hella&songCount=2&albumCount=0&artistCount=0")
    got = first["searchResult3"]["song"]
    assert [x["id"] for x in got] == ["L0", "L1", "deezer:1001"]
    second = await world.json(
        "search3", f"query=hella&songCount=2&songOffset={len(got)}&albumCount=0&artistCount=0"
    )
    assert [x["id"] for x in second["searchResult3"]["song"]] == ["L2", "L3"]


async def test_empty_query_sync_is_passthrough(world):
    world.navidrome.songs["L1"] = song("L1", "A", "B")
    root = await world.json("search3", "query=&songCount=10")
    assert [x["id"] for x in root["searchResult3"]["song"]] == ["L1"]
    assert not any(h.startswith("/search") for h in world.deezer.hits)


# ---------------------------------------------------------------- browsing
async def test_virtual_album_and_artist(world):
    album = (await world.json("getAlbum", "id=deezer-album:100"))["album"]
    assert album["songCount"] == 3 and [x["track"] for x in album["song"]] == [1, 2, 3]
    assert album["song"][0]["artist"] == "TVETH, Bato"
    artist = (await world.json("getArtist", "id=deezer-artist:1"))["artist"]
    assert [a["id"] for a in artist["album"]] == ["deezer-album:100"]
    info = (await world.json("getArtistInfo2", "id=deezer-artist:1"))["artistInfo2"]
    assert info["largeImageUrl"]


# ------------------------------------------------------------------- playback
async def test_first_play_downloads_then_streams_with_ranges(world):
    world.finder.results["Hella Quality"] = [candidate("dEvrb6N70kA", "Hella Quality")]
    resp = await world.get("stream", "id=deezer:1001", headers={"Range": "bytes=0-1"})
    assert resp.status == 206
    assert resp.headers["Content-Range"].startswith("bytes 0-1/")
    assert resp.headers["Content-Type"] == "audio/mp4"
    total = int(resp.headers["Content-Range"].rsplit("/", 1)[1])
    full = await world.get("stream", "id=deezer:1001")
    body = await full.read()
    assert len(body) == total and body.find(b"moov") < body.find(b"mdat")  # faststart, no DASH
    assert world.downloader.calls == ["dEvrb6N70kA"]
    song_obj = (await world.json("getSong", "id=deezer:1001"))["song"]
    assert song_obj["size"] == total and song_obj["duration"] == 30
    # Other tracks of the album are prefetched in the priority lane.
    await until(lambda: world.services.store.get_job("deezer:1002", "download") is not None)


async def test_unavailable_track_returns_error_quickly(world):
    resp = await world.json("stream", "id=deezer:1002")
    assert resp["status"] == "failed"
    assert "no matching audio" in resp["error"]["message"]


async def test_downloaded_track_becomes_local_everywhere(world):
    world.finder.results["Hella Quality"] = [candidate("dEvrb6N70kA", "Hella Quality")]
    await (await world.get("stream", "id=deezer:1001")).read()
    store = world.services.store
    await until(lambda: store.get_track("deezer:1001").local_id)
    local_id = store.get_track("deezer:1001").local_id
    root = await world.json("search3", "query=hella&songCount=20&albumCount=0&artistCount=0")
    assert [x["id"] for x in root["searchResult3"]["song"]] == [local_id]  # no virtual twin
    album = (await world.json("getAlbum", "id=deezer-album:100"))["album"]
    assert album["song"][0]["id"] == local_id
    assert [x["id"] for x in album["song"][1:]] == ["deezer:1002", "deezer:1003"]


async def test_local_album_is_completed_with_missing_catalog_tracks(world):
    world.finder.results["Hella Quality"] = [candidate("dEvrb6N70kA", "Hella Quality")]
    await (await world.get("stream", "id=deezer:1001")).read()
    store = world.services.store
    await until(lambda: store.get_track("deezer:1001").local_id)
    local = world.navidrome.songs[store.get_track("deezer:1001").local_id]
    await until(lambda: world.services.deezer.cached_album_tracks("100"))
    album = (await world.json("getAlbum", f"id={local['albumId']}"))["album"]
    assert [x["id"] for x in album["song"]] == [local["id"], "deezer:1002", "deezer:1003"]
    assert album["songCount"] == 3


# ---------------------------------------------------------------------- likes
async def test_like_shows_immediately_then_lands_in_navidrome(world):
    world.finder.results["Second Song"] = [candidate("SSSSSSSSSSS", "Second Song")]
    assert (await world.json("star", "id=deezer:1002"))["status"] == "ok"
    starred = (await world.json("getStarred2"))["starred2"]["song"]
    assert [x["id"] for x in starred] == ["deezer:1002"] and starred[0]["starred"].endswith("Z")
    # Other users never see it.
    assert "song" not in (await world.json("getStarred2", auth=BOB)).get("starred2", {})
    store = world.services.store
    await until(lambda: store.get_track("deezer:1002").local_id)
    await until(lambda: not store.pending_stars("alice"))
    local_id = store.get_track("deezer:1002").local_id
    assert world.navidrome.stars["alice"] == {local_id}
    starred = (await world.json("getStarred2"))["starred2"]["song"]
    assert [x["id"] for x in starred] == [local_id]


async def test_unstar_before_download_forgets_intent(world):
    await world.json("star", "id=deezer:1003")
    await world.json("unstar", "id=deezer:1003")
    assert world.services.store.pending_stars("alice") == []


async def test_mixed_star_forwards_local_ids(world):
    world.navidrome.songs["L1"] = song("L1", "Local", "X")
    await world.json("star", "id=L1&id=deezer:1003")
    assert world.navidrome.stars["alice"] == {"L1"}
    assert [t for _, t, _ in world.services.store.pending_stars("alice")] == ["deezer:1003"]


# ----------------------------------------------------- scrobbles & playlists
async def test_scrobble_of_external_track_is_delivered_after_download(world):
    world.finder.results["Hella Quality"] = [candidate("dEvrb6N70kA", "Hella Quality")]
    assert (await world.json("scrobble", "id=deezer:1001&time=1700000000000&submission=true"))[
        "status"
    ] == "ok"
    await (await world.get("stream", "id=deezer:1001")).read()
    store = world.services.store
    await until(lambda: store.get_track("deezer:1001").local_id)
    await until(lambda: world.navidrome.scrobbles)
    assert world.navidrome.scrobbles[0][:2] == ("alice", store.get_track("deezer:1001").local_id)


async def test_playlist_add_of_external_track(world):
    world.finder.results["Third Song"] = [candidate("TTTTTTTTTTT", "Third Song")]
    root = await world.json("createPlaylist", "name=Mix&songId=deezer:1003")
    playlist_id = root["playlist"]["id"]
    shown = (await world.json("getPlaylist", f"id={playlist_id}"))["playlist"]["entry"]
    assert [x["id"] for x in shown] == ["deezer:1003"]
    store = world.services.store
    await until(lambda: store.get_track("deezer:1003").local_id)
    await until(lambda: world.navidrome.playlists[playlist_id]["songs"])
    assert world.navidrome.playlists[playlist_id]["songs"] == [store.get_track("deezer:1003").local_id]

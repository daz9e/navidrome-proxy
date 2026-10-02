import asyncio

import pytest

from navidrome_proxy import imports

from .conftest import candidate
from .fakes import dz_track


@pytest.mark.parametrize(
    ("line", "expected"),
    [
        ("1. TVETH, BATO - Hella Quality", {"artist": "TVETH, BATO", "title": "Hella Quality"}),
        ("12) Юпи – Порезы на руках", {"artist": "Юпи", "title": "Порезы на руках"}),
        (
            "Imagine Dragons - Radioactive [Night Visions] (3:07)",
            {"artist": "Imagine Dragons", "title": "Radioactive", "album": "Night Visions", "duration": 187},
        ),
        ("Jay-Z - 99 Problems", {"artist": "Jay-Z", "title": "99 Problems"}),
        ("A - B - C", {"artist": "A", "title": "B - C"}),
    ],
)
def test_parse_line(line, expected):
    parsed = imports.parse_line(line)
    assert {k: v for k, v in parsed.items() if v is not None} == expected


def test_parse_text_marks_invalid_and_skips_blank():
    rows = imports.parse_text("A - B\n\njust words\n")
    assert [(r["line"], r["status"]) for r in rows] == [(1, "pending"), (3, "invalid")]
    with pytest.raises(ValueError):
        imports.parse_text("\n \n")


async def wait_import(world, import_id, auth="u=alice&p=alicepw"):
    for _ in range(300):
        resp = await world.client.get(f"/proxy/v1/imports/{import_id}?{auth}")
        report = await resp.json()
        if report.get("status") == "completed":
            return report
        await asyncio.sleep(0.02)
    raise AssertionError("import did not finish")


async def test_import_matches_collabs_editions_and_other_sources(world):
    dz = world.deezer
    dz.add_album(
        200,
        "Night Visions",
        "Imagine Dragons",
        5,
        [dz_track(63510358, "Radioactive", "Imagine Dragons", 200, "Night Visions", 187, 5)],
    )
    dz.add_album(
        201,
        "Radioactive",
        "Imagine Dragons",
        5,
        [dz_track(74506420, "Radioactive", "Imagine Dragons", 201, "Radioactive", 276, 5)],
    )
    world.finder.results["Порезы на руках"] = [candidate("Q1QvyIrUzz0", "Порезы на руках", "Юпи")]
    text = "\n".join(
        [
            "1. TVETH, BATO - Hella Quality",
            "2. Imagine Dragons - Radioactive [Night Visions]",
            "3. Юпи - Порезы на руках",
            "4. Nobody - Nothing At All",
            "5. garbage line",
        ]
    )
    resp = await world.client.post(
        "/proxy/v1/imports?u=alice&p=alicepw", data=text.encode(), headers={"Content-Type": "text/plain"}
    )
    assert resp.status == 202
    report = await wait_import(world, (await resp.json())["id"])
    rows = {r["line"]: r for r in report["rows"]}
    assert (rows[1]["status"], rows[1]["track_id"]) == ("matched", "deezer:1001")
    assert (rows[2]["status"], rows[2]["track_id"]) == ("matched", "deezer:63510358")
    assert rows[2]["alternatives"][0]["id"] == "deezer:74506420"
    assert (rows[3]["status"], rows[3]["track_id"]) == ("matched_youtube", "yt:Q1QvyIrUzz0")
    assert rows[4]["status"] == "not_found" and rows[5]["status"] == "invalid"
    liked = {t for _, t, _ in world.services.store.pending_stars("alice")}
    assert liked == {"deezer:1001", "deezer:63510358", "yt:Q1QvyIrUzz0"}
    # Import downloads use the background lane.
    job = world.services.store.get_job("deezer:1001", "download")
    assert job.priority == 2 and job.reason == "like-import:alice"


async def test_import_reports_are_private(world):
    resp = await world.client.post(
        "/proxy/v1/imports",
        json={"u": "alice", "p": "alicepw", "text": "TVETH - Hella Quality", "like": False},
    )
    import_id = (await resp.json())["id"]
    await wait_import(world, import_id)
    other = await world.client.get(f"/proxy/v1/imports/{import_id}?u=bob&p=bobpw")
    assert other.status == 404
    assert (await world.client.get(f"/proxy/v1/imports/{import_id}?u=bob&p=wrong")).status == 401
    assert world.services.store.pending_stars("alice") == []  # like=false


async def test_import_playlist_keeps_list_order(world):
    for title, ref in (
        ("Hella Quality", "AAAAAAAAAAA"),
        ("Second Song", "BBBBBBBBBBB"),
        ("Third Song", "CCCCCCCCCCC"),
    ):
        world.finder.results[title] = [candidate(ref, title)]
    text = "TVETH - Third Song\nTVETH - Hella Quality\nTVETH - Second Song"
    resp = await world.client.post(
        "/proxy/v1/imports", json={"u": "alice", "p": "alicepw", "text": text, "playlist": "Mix"}
    )
    import_id = (await resp.json())["id"]
    await wait_import(world, import_id)
    store = world.services.store
    for _ in range(500):
        if all(store.get_track(t).local_id for t in ("deezer:1001", "deezer:1002", "deezer:1003")):
            break
        await asyncio.sleep(0.02)
    await world.services.extra["importer"].sync_playlists()
    report = await wait_import(world, import_id)
    playlist = world.navidrome.playlists[report["playlist_id"]]["songs"]
    expected = [store.get_track(t).local_id for t in ("deezer:1003", "deezer:1001", "deezer:1002")]
    assert playlist == expected


async def test_import_page_is_served(world):
    page = await world.client.get("/proxy/import")
    assert page.status == 200 and "Import tracks" in await page.text()


async def test_title_only_match_from_another_artist_is_not_taken(world):
    world.deezer.add_album(
        300,
        "If The Sky Came Down",
        "Lost Society",
        30,
        [dz_track(1725700, "Awake", "Lost Society", 300, "If", 200, 30)],
    )
    world.finder.results["Walking"] = [candidate("WWWWWWWWWWW", "Walking", "russianmafiaextra")]
    resp = await world.client.post(
        "/proxy/v1/imports",
        json={"u": "alice", "p": "alicepw", "text": "russianmafiaextra - awake\nrussianmafiaextra - Walking"},
    )
    report = await wait_import(world, (await resp.json())["id"])
    rows = {r["line"]: r for r in report["rows"]}
    assert rows[1]["status"] == "not_found" and "track_id" not in rows[1]
    assert rows[2]["status"] == "matched_youtube"
    assert {t for _, t, _ in world.services.store.pending_stars("alice")} == {"yt:WWWWWWWWWWW"}


async def test_recheck_takes_back_a_wrong_like_and_matches_again(world):
    store = world.services.store
    world.deezer.add_album(
        300,
        "If The Sky Came Down",
        "Lost Society",
        30,
        [dz_track(1725700, "Awake", "Lost Society", 300, "If", 200, 30)],
    )
    store.remember_track("deezer:1725700", {"title": "Awake", "artist": "Lost Society"})
    store.update_track("deezer:1725700", status="ready", local_id="WRONG1")
    world.navidrome.songs["WRONG1"] = {
        "id": "WRONG1",
        "title": "Awake",
        "artist": "Lost Society",
        "album": "If",
    }
    world.navidrome.stars["alice"] = {"WRONG1"}
    rows = imports.parse_text("russianmafiaextra - awake\nTVETH - Hella Quality")
    rows[0].update(
        status="fallback",
        track_id="deezer:1725700",
        match={"id": "deezer:1725700", "artist": "Lost Society", "title": "Awake", "score": 52.5},
    )
    rows[1].update(
        status="matched", track_id="deezer:1001", match={"artist": "TVETH, Bato", "title": "Hella Quality"}
    )
    store.save_import(
        "old", "alice", "completed", {"like": True, "playlist": None, "playlist_id": None}, rows
    )
    importer = world.services.extra["importer"]

    dry = importer.recheck("old", dry_run=True)
    assert dry["wrong"] == 1 and store.pending_unstars("alice") == []
    assert importer.recheck("old") == {"wrong": 1, "unstar": 1, "lines": dry["lines"]}
    assert store.pending_unstars("alice") == ["WRONG1"]

    await world.json("search3", "query=nothing")  # alice shows up; her credentials are known again
    for _ in range(200):  # that request replays her intents in the background
        if not world.navidrome.stars["alice"]:
            break
        await asyncio.sleep(0.02)
    assert world.navidrome.stars["alice"] == set()
    assert store.pending_unstars("alice") == []

    importer._queue.put_nowait("old")
    report = await wait_import(world, "old")
    assert [r["status"] for r in report["rows"]] == ["not_found", "matched"]


async def test_import_accepts_token_auth_like_the_import_page_sends(world):
    import hashlib

    salt = "c0ffee"
    auth = {"u": "alice", "t": hashlib.md5(("alicepw" + salt).encode()).hexdigest(), "s": salt}
    resp = await world.client.post("/proxy/v1/imports", json={**auth, "text": "TVETH - Hella Quality"})
    assert resp.status == 202
    report = await wait_import(world, (await resp.json())["id"], auth=f"u=alice&t={auth['t']}&s={salt}")
    assert report["status"] == "completed"

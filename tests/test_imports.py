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

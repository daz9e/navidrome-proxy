from navidrome_proxy.reconcile import Reconciler

from .fakes import dz_track


async def test_youtube_download_is_linked_to_its_later_deezer_release(world, tmp_path):
    s = world.services
    f = world.music / "song.m4a"
    f.write_bytes(b"x")
    s.store.remember_track("yt:AAAAAAAAAAA", {"title": "Brand New", "artists": ["TVETH"], "duration": 120})
    s.store.update_track("yt:AAAAAAAAAAA", status="ready", file=str(f), audio={"duration": 121})
    s.store.remember_track("yt:BBBBBBBBBBB", {"title": "Unknown", "artists": ["Nobody"], "duration": 100})
    s.store.update_track("yt:BBBBBBBBBBB", status="ready", file=str(f), audio={"duration": 100})
    world.deezer.add_album(
        300, "Brand New", "TVETH", 1, [dz_track(3001, "Brand New", "TVETH", 300, "Brand New", 122)]
    )
    world.deezer.add_album(
        301, "Other", "TVETH", 1, [dz_track(3002, "Brand New", "TVETH", 301, "Other", 180)]
    )
    reconciler = Reconciler(s.store, s.identity, s.deezer)
    assert await reconciler.run_once() == 1
    assert s.identity.canonical("deezer:3001") == "yt:AAAAAAAAAAA"
    assert s.identity.canonical("deezer:3002") == "deezer:3002"  # different length: another version
    assert await reconciler.run_once() == 0  # remembered, not rechecked immediately

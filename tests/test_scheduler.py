import asyncio

import pytest

from navidrome_proxy import scheduler as sched
from navidrome_proxy.db import Store
from navidrome_proxy.sources import SourceError


@pytest.fixture
def store(tmp_path):
    s = Store(tmp_path / "db.sqlite3")
    yield s
    s.close()


def ready_track(store, track_id, tmp_path):
    f = tmp_path / f"{track_id.replace(':', '_')}.m4a"
    f.write_bytes(b"x")
    store.update_track(track_id, status="ready", file=str(f))
    return store.get_track(track_id)


async def until(predicate, timeout=3.0):
    for _ in range(int(timeout / 0.01)):
        if predicate():
            return
        await asyncio.sleep(0.01)
    raise AssertionError("condition not reached")


async def test_priority_lane_never_waits_for_busy_background_workers(store, tmp_path):
    gate = asyncio.Event()
    running, peak = set(), {"bg": 0}

    async def handler(job, cancel):
        running.add(job.track_id)
        if job.priority >= sched.BACKGROUND:
            peak["bg"] = max(peak["bg"], sum(1 for t in running if t.startswith("deezer:bg")))
            await gate.wait()
        running.discard(job.track_id)
        return ready_track(store, job.track_id, tmp_path)

    s = sched.Scheduler(store, background_workers=5)
    s.handlers["download"] = handler
    for i in range(12):
        store.remember_track(f"deezer:bg{i}", {"title": str(i)})
        s.request(f"deezer:bg{i}", sched.BACKGROUND, "import")
    s.start()
    await until(lambda: len(running) == 5)
    store.remember_track("deezer:play", {"title": "now"})
    s.request("deezer:play", sched.PLAY, "play")
    track = await s.wait("deezer:play", timeout=2)
    assert track is not None and track.id == "deezer:play"
    assert peak["bg"] == 5
    gate.set()
    await until(lambda: store.job_counts().get("download:done") == 13)
    assert peak["bg"] == 5
    await s.stop()


async def test_duplicate_requests_run_once_and_play_joins_running_background_job(store, tmp_path):
    calls = []
    gate = asyncio.Event()

    async def handler(job, cancel):
        calls.append(job.track_id)
        await gate.wait()
        return ready_track(store, job.track_id, tmp_path)

    s = sched.Scheduler(store, background_workers=1)
    s.handlers["download"] = handler
    store.remember_track("deezer:1", {"title": "x"})
    s.request("deezer:1", sched.BACKGROUND, "album")
    s.start()
    await until(lambda: calls == ["deezer:1"])
    assert not s.request("deezer:1", sched.PLAY, "play")  # already running: not queued twice
    waiter = asyncio.create_task(s.wait("deezer:1", timeout=2))
    await asyncio.sleep(0.05)
    gate.set()
    assert (await waiter).id == "deezer:1"
    assert calls == ["deezer:1"]
    await s.stop()


async def test_transient_failures_retry_then_fail_and_fresh_request_revives(store, tmp_path, monkeypatch):
    monkeypatch.setattr(sched, "BACKOFF", (0, 0, 0))
    monkeypatch.setattr(sched, "MAX_ATTEMPTS", 3)
    attempts = []

    async def handler(job, cancel):
        attempts.append(job.attempts)
        raise SourceError("HTTP 403", transient=True)

    s = sched.Scheduler(store, background_workers=1)
    s.handlers["download"] = handler
    store.remember_track("deezer:1", {"title": "x"})
    s.request("deezer:1", sched.LIKE, "like")
    s.start()
    await until(lambda: store.get_job("deezer:1", "download").state == "failed")
    assert attempts == [1, 2, 3]
    assert store.get_track("deezer:1").status == "failed"
    s.request("deezer:1", sched.PLAY, "play", fresh=True)
    await until(lambda: len(attempts) == 6)
    await s.stop()


async def test_permanent_failure_is_not_retried_and_waiters_get_none(store):
    async def handler(job, cancel):
        raise SourceError("no matching audio", transient=False)

    s = sched.Scheduler(store, background_workers=1)
    s.handlers["download"] = handler
    store.remember_track("deezer:1", {"title": "x"})
    s.request("deezer:1", sched.PLAY, "play")
    s.start()
    assert await s.wait("deezer:1", timeout=2) is None
    job = store.get_job("deezer:1", "download")
    assert (job.state, job.attempts) == ("failed", 1)
    assert "no matching audio" in store.get_track("deezer:1").error
    await s.stop()


async def test_wait_times_out_and_ready_tracks_return_immediately(store, tmp_path):
    s = sched.Scheduler(store, background_workers=1)
    store.remember_track("deezer:1", {"title": "x"})
    ready_track(store, "deezer:1", tmp_path)
    assert (await s.wait("deezer:1", timeout=0.1)).id == "deezer:1"
    store.remember_track("deezer:2", {"title": "y"})
    store.enqueue("deezer:2", "download", 2)
    assert await s.wait("deezer:2", timeout=0.05) is None


async def test_done_hooks_receive_the_backing_track(store, tmp_path):
    seen = []

    async def handler(job, cancel):
        return ready_track(store, job.track_id, tmp_path)

    async def hook(job, track):
        seen.append((job.reason, track.id))

    s = sched.Scheduler(store, background_workers=1)
    s.handlers["download"] = handler
    s.on_done.append(hook)
    store.remember_track("deezer:1", {"title": "x"})
    s.request("deezer:1", sched.LIKE, "like:alice")
    s.start()
    await until(lambda: seen == [("like:alice", "deezer:1")])
    await s.stop()

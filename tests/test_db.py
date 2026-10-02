import sqlite3
import threading
import time

import pytest

from navidrome_proxy import db
from navidrome_proxy.db import Store


@pytest.fixture
def store(tmp_path):
    s = Store(tmp_path / "p.sqlite3")
    yield s
    s.close()


def test_remember_track_merges_metadata_and_keeps_state(store):
    store.remember_track("deezer:1", {"title": "A", "artist": "X", "album_id": "10", "duration": 100})
    store.update_track("deezer:1", status="ready", file="/music/deezer-1.m4a", local_id="L1")
    t = store.remember_track("deezer:1", {"title": "A (Remastered)", "cover": None})
    assert t.title == "A (Remastered)"
    assert t.artist == "X"
    assert t.status == "ready"
    assert t.local_id == "L1"
    assert t.album_id == "10"
    assert "cover" not in t.meta
    assert store.track_by_local_id("L1").id == "deezer:1"
    assert [x.id for x in store.tracks_in_album("10")] == ["deezer:1"]


def test_update_track_rejects_unknown_columns(store):
    store.remember_track("deezer:1", {})
    with pytest.raises(ValueError):
        store.update_track("deezer:1", status="ready", id="evil")


def test_audio_is_stored_as_json(store):
    store.remember_track("deezer:1", {})
    store.update_track("deezer:1", audio={"duration": 12.5, "suffix": "m4a"})
    assert store.get_track("deezer:1").audio == {"duration": 12.5, "suffix": "m4a"}


def test_enqueue_dedupes_and_raises_priority(store):
    assert store.enqueue("deezer:1", "download", 2, "album")
    assert not store.enqueue("deezer:1", "download", 2, "album")
    assert store.enqueue("deezer:1", "download", 0, "play")
    job = store.get_job("deezer:1", "download")
    assert job.priority == 0
    assert job.state == "queued"


def test_claim_respects_priority_order_and_bounds(store):
    store.enqueue("deezer:bg", "download", 2)
    time.sleep(0.001)
    store.enqueue("deezer:play", "download", 0)
    store.enqueue("deezer:star", "download", 1)
    assert store.claim_next_job(max_priority=1).track_id == "deezer:play"
    assert store.claim_next_job(min_priority=2).track_id == "deezer:bg"
    assert store.claim_next_job(min_priority=2) is None
    assert store.claim_next_job().track_id == "deezer:star"
    assert store.claim_next_job() is None


def test_running_job_is_not_restarted_but_can_be_reprioritized(store):
    store.enqueue("deezer:1", "download", 2)
    store.claim_next_job()
    assert not store.enqueue("deezer:1", "download", 0)
    job = store.get_job("deezer:1", "download")
    assert (job.state, job.priority) == ("running", 0)


def test_retry_backoff_and_revival(store):
    store.enqueue("deezer:1", "download", 2)
    store.claim_next_job()
    store.finish_job("deezer:1", "download", "queued", error="timeout", retry_at=time.time() + 60)
    assert store.claim_next_job() is None
    assert store.claim_next_job(now=time.time() + 61).track_id == "deezer:1"
    store.finish_job("deezer:1", "download", "failed", error="nope")
    # An explicit play revives a failed job with a fresh attempt budget.
    assert store.enqueue("deezer:1", "download", 0, "play", reset_attempts=True)
    job = store.get_job("deezer:1", "download")
    assert (job.state, job.attempts, job.error) == ("queued", 0, None)


def test_requeue_running_after_restart(store):
    store.enqueue("deezer:1", "download", 2)
    store.claim_next_job()
    assert store.requeue_running_jobs() == 1
    assert store.get_job("deezer:1", "download").state == "queued"


def test_pending_intents_are_per_user(store):
    store.add_pending_star("alice", "deezer:1")
    store.add_pending_star("alice", "deezer:1")
    store.add_pending_star("bob", "deezer:2")
    assert [t for _, t, _ in store.pending_stars("alice")] == ["deezer:1"]
    assert store.remove_pending_star("alice", "deezer:1")
    assert not store.remove_pending_star("alice", "deezer:1")
    assert [t for _, t, _ in store.pending_stars("bob")] == ["deezer:2"]
    store.add_pending_scrobble("bob", "deezer:2", 100, True)
    store.add_pending_playlist_add("carol", "pl1", "deezer:3")
    assert store.users_with_pending_work() == {"bob", "carol"}


def test_cache_expires(store):
    store.cache_set("k", {"a": 1}, ttl=60)
    assert store.cache_get("k") == {"a": 1}
    store.cache_set("old", 1, ttl=-1)
    assert store.cache_get("old") is None
    assert store.cache_purge_expired() == 1


def test_imports_round_trip(store):
    store.save_import("i1", "alice", "processing", {"like": True}, [{"line": 1}])
    store.save_import("i1", "alice", "completed", {"like": True}, [{"line": 1, "status": "queued"}])
    imp = store.get_import("i1")
    assert imp["status"] == "completed"
    assert imp["rows"][0]["status"] == "queued"
    assert [i["id"] for i in store.list_imports("alice")] == ["i1"]
    assert store.list_imports("bob") == []


def test_concurrent_writers_do_not_corrupt(store):
    def writer(n):
        for i in range(50):
            store.remember_track(f"deezer:{n}{i}", {"title": str(i)})
            store.enqueue(f"deezer:{n}{i}", "download", 2)

    threads = [threading.Thread(target=writer, args=(n,)) for n in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert sum(store.job_counts().values()) == 400


def test_integrity_reports_healthy_and_corrupt_databases(tmp_path):
    path = tmp_path / "x.sqlite3"
    store = Store(path)
    for i in range(300):
        store.remember_track(f"deezer:{i}", {"title": "t" * 200})
    assert store.integrity() == [] and store.integrity(quick=False) == []
    store.close()
    data = bytearray(path.read_bytes())
    data[4096 * 3 : 4096 * 3 + 64] = b"\xff" * 64
    path.write_bytes(bytes(data))
    broken = Store.__new__(Store)
    import sqlite3
    import threading

    broken._lock = threading.RLock()
    broken._conn = sqlite3.connect(str(path))
    assert broken.integrity(quick=False)


def test_owner_lock_admits_one_process_at_a_time(tmp_path):
    from navidrome_proxy.db import DatabaseBusy, OwnerLock

    first = OwnerLock(tmp_path / "p.sqlite3")
    first.acquire()
    with pytest.raises(DatabaseBusy):
        OwnerLock(tmp_path / "p.sqlite3").acquire()
    first.release()
    with OwnerLock(tmp_path / "p.sqlite3"):
        pass


def test_owner_lock_holds_across_processes(tmp_path):
    import subprocess
    import sys

    from navidrome_proxy.db import OwnerLock

    with OwnerLock(tmp_path / "p.sqlite3"):
        code = (
            "import sys; from navidrome_proxy.db import OwnerLock, DatabaseBusy\n"
            f"try: OwnerLock({str(tmp_path / 'p.sqlite3')!r}).acquire()\n"
            "except DatabaseBusy: sys.exit(3)\n"
        )
        assert subprocess.run([sys.executable, "-c", code]).returncode == 3


def test_backup_is_verified_and_rotated(store, tmp_path):
    store.remember_track("deezer:1", {"title": "A"})
    backups = tmp_path / "backups"
    made = []
    for i in range(4):
        (backups / f"proxy-2026010{i}-000000.sqlite3").parent.mkdir(exist_ok=True)
        (backups / f"proxy-2026010{i}-000000.sqlite3").write_bytes(b"old")
    made.append(store.backup(backups, keep=3))
    assert made[0] is not None and made[0].exists()
    kept = sorted(p.name for p in backups.glob("proxy-*.sqlite3"))
    assert len(kept) == 3 and kept[-1] == made[0].name
    copy = Store(made[0])
    assert copy.get_track("deezer:1").title == "A"
    copy.close()


def test_backup_is_skipped_for_a_corrupt_database(tmp_path):
    import sqlite3

    path = tmp_path / "x.sqlite3"
    s = Store(path)
    for i in range(300):
        s.remember_track(f"deezer:{i}", {"title": "t" * 200})
    s.close()
    data = bytearray(path.read_bytes())
    data[4096 * 3 : 4096 * 3 + 64] = b"\xff" * 64
    path.write_bytes(bytes(data))
    broken = Store.__new__(Store)
    broken._lock = threading.RLock()
    broken._conn = sqlite3.connect(str(path))
    assert broken.backup(tmp_path / "backups") is None
    assert not list((tmp_path / "backups").glob("*")) if (tmp_path / "backups").exists() else True


def test_schema_migrations_run_once_in_order(tmp_path, monkeypatch):
    path = tmp_path / "m.sqlite3"
    Store(path).close()
    monkeypatch.setattr(db, "SCHEMA_VERSION", 3)
    monkeypatch.setattr(
        db,
        "MIGRATIONS",
        {
            2: ("ALTER TABLE tracks ADD COLUMN rating INTEGER",),
            3: ("UPDATE tracks SET rating = 5", "CREATE INDEX tracks_rating ON tracks(rating)"),
        },
    )
    s = Store(path)
    assert s.get_meta("schema_version") == "3"
    s.close()
    Store(path).close()  # already at 3: nothing runs twice


def test_newer_schema_is_refused(tmp_path):
    path = tmp_path / "n.sqlite3"
    s = Store(path)
    s.set_meta("schema_version", "99")
    s.close()
    with pytest.raises(db.SchemaTooNew):
        Store(path)


def test_failed_migration_leaves_the_old_version(tmp_path, monkeypatch):
    path = tmp_path / "f.sqlite3"
    Store(path).close()
    monkeypatch.setattr(db, "SCHEMA_VERSION", 2)
    monkeypatch.setattr(db, "MIGRATIONS", {2: ("ALTER TABLE tracks ADD COLUMN x INTEGER", "NOT SQL")})
    with pytest.raises(sqlite3.OperationalError):
        Store(path)
    monkeypatch.setattr(db, "SCHEMA_VERSION", 1)
    s = Store(path)
    assert s.get_meta("schema_version") == "1"
    assert "x" not in [r[1] for r in s._query("PRAGMA table_info(tracks)")]
    s.close()

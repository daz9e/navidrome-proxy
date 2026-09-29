"""Durable proxy state in SQLite.

The first proxy generation rewrote a 17 MB ``state.json`` on every change, which
cost seconds per request and corrupted the file on crashes. Here every mutation is
a small transaction in a WAL-mode database.

Navidrome stays the owner of the library and of likes/playlists; the proxy only
stores what Navidrome cannot know: external metadata, the mapping from an external
track to its file / Navidrome song id, the download queue and user intents that
wait for a track to become local.
"""

from __future__ import annotations

import json
import sqlite3
import threading
import time
from collections.abc import Iterable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1

_SCHEMA = """
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS tracks (
    id TEXT PRIMARY KEY,
    album_id TEXT,
    meta TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'new',
    file TEXT,
    local_id TEXT,
    source TEXT,
    source_ref TEXT,
    audio TEXT,
    error TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS tracks_album ON tracks(album_id);
CREATE INDEX IF NOT EXISTS tracks_local ON tracks(local_id);
CREATE INDEX IF NOT EXISTS tracks_status ON tracks(status);

-- Other identities of the same recording: "deezer:1", "yt:abc", "sc:2", "isrc:USUM1".
CREATE TABLE IF NOT EXISTS aliases (
    alias TEXT PRIMARY KEY,
    track_id TEXT NOT NULL,
    how TEXT NOT NULL,
    created_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS aliases_track ON aliases(track_id);

-- "primary artist|base title" keys, for spotting one song under different ids.
CREATE TABLE IF NOT EXISTS recordings (
    key TEXT NOT NULL,
    track_id TEXT NOT NULL,
    duration REAL NOT NULL,
    PRIMARY KEY (key, track_id)
);

CREATE TABLE IF NOT EXISTS jobs (
    track_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    priority INTEGER NOT NULL,
    state TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    not_before REAL NOT NULL DEFAULT 0,
    enqueued_at REAL NOT NULL,
    reason TEXT,
    error TEXT,
    PRIMARY KEY (track_id, kind)
);
CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state, priority, enqueued_at);

CREATE TABLE IF NOT EXISTS pending_stars (
    username TEXT NOT NULL,
    target TEXT NOT NULL,
    created_at REAL NOT NULL,
    PRIMARY KEY (username, target)
);

CREATE TABLE IF NOT EXISTS pending_scrobbles (
    username TEXT NOT NULL,
    track_id TEXT NOT NULL,
    time INTEGER NOT NULL,
    submission INTEGER NOT NULL,
    PRIMARY KEY (username, track_id, time, submission)
);

CREATE TABLE IF NOT EXISTS pending_playlist_adds (
    username TEXT NOT NULL,
    playlist_id TEXT NOT NULL,
    track_id TEXT NOT NULL,
    created_at REAL NOT NULL,
    PRIMARY KEY (username, playlist_id, track_id)
);

CREATE TABLE IF NOT EXISTS cache (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    expires_at REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS imports (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    status TEXT NOT NULL,
    options TEXT NOT NULL DEFAULT '{}',
    rows TEXT NOT NULL DEFAULT '[]',
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS imports_user ON imports(username, created_at);
"""

TERMINAL_TRACK_STATES = ("ready", "failed")


@dataclass
class Track:
    id: str
    meta: dict[str, Any] = field(default_factory=dict)
    status: str = "new"
    file: str | None = None
    local_id: str | None = None
    source: str | None = None
    source_ref: str | None = None
    audio: dict[str, Any] = field(default_factory=dict)
    error: str | None = None
    attempts: int = 0
    album_id: str | None = None
    created_at: float = 0.0
    updated_at: float = 0.0

    @property
    def title(self) -> str:
        return str(self.meta.get("title") or "")

    @property
    def artist(self) -> str:
        return str(self.meta.get("artist") or "")

    @property
    def artists(self) -> list[str]:
        value = self.meta.get("artists")
        if isinstance(value, list) and value:
            return [str(v) for v in value]
        return [self.artist] if self.artist else []

    @property
    def duration(self) -> float:
        try:
            return float(self.meta.get("duration") or 0)
        except (TypeError, ValueError):
            return 0.0

    @property
    def is_ready(self) -> bool:
        return self.status == "ready" and bool(self.file) and Path(self.file).is_file()


@dataclass
class Job:
    track_id: str
    kind: str
    priority: int
    state: str
    attempts: int = 0
    not_before: float = 0.0
    enqueued_at: float = 0.0
    reason: str | None = None
    error: str | None = None


def _track(row: sqlite3.Row) -> Track:
    return Track(
        id=row["id"],
        meta=json.loads(row["meta"] or "{}"),
        status=row["status"],
        file=row["file"],
        local_id=row["local_id"],
        source=row["source"],
        source_ref=row["source_ref"],
        audio=json.loads(row["audio"] or "{}"),
        error=row["error"],
        attempts=row["attempts"],
        album_id=row["album_id"],
        created_at=row["created_at"],
        updated_at=row["updated_at"],
    )


def _job(row: sqlite3.Row) -> Job:
    return Job(**{k: row[k] for k in row.keys()})  # noqa: SIM118 - sqlite3.Row is not a Mapping


class Store:
    """Thread-safe facade over one SQLite connection.

    All statements are short; they are executed synchronously from the event loop
    (WAL + ``synchronous=NORMAL`` keeps them in the sub-millisecond range) and from
    worker threads, serialized by one lock.
    """

    def __init__(self, path: Path | str):
        self.path = Path(path)
        if str(path) != ":memory:":
            self.path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.RLock()
        self._conn = sqlite3.connect(str(path), check_same_thread=False, isolation_level=None)
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute("PRAGMA synchronous=NORMAL")
        self._conn.execute("PRAGMA foreign_keys=ON")
        with self._lock:
            self._conn.executescript(_SCHEMA)
        with self.tx() as c:
            c.execute(
                "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?)", (str(SCHEMA_VERSION),)
            )

    def close(self) -> None:
        with self._lock:
            self._conn.close()

    @contextmanager
    def tx(self) -> Iterator[sqlite3.Connection]:
        with self._lock:
            self._conn.execute("BEGIN IMMEDIATE")
            try:
                yield self._conn
            except BaseException:
                self._conn.execute("ROLLBACK")
                raise
            else:
                self._conn.execute("COMMIT")

    def _query(self, sql: str, args: Iterable[Any] = ()) -> list[sqlite3.Row]:
        with self._lock:
            return self._conn.execute(sql, tuple(args)).fetchall()

    # ------------------------------------------------------------------ meta
    def get_meta(self, key: str) -> str | None:
        rows = self._query("SELECT value FROM meta WHERE key = ?", (key,))
        return rows[0]["value"] if rows else None

    def set_meta(self, key: str, value: str) -> None:
        with self.tx() as c:
            c.execute(
                "INSERT INTO meta(key, value) VALUES (?, ?) "
                "ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (key, value),
            )

    # ---------------------------------------------------------------- tracks
    def get_track(self, track_id: str) -> Track | None:
        rows = self._query("SELECT * FROM tracks WHERE id = ?", (track_id,))
        return _track(rows[0]) if rows else None

    def get_tracks(self, track_ids: Iterable[str]) -> dict[str, Track]:
        ids = list(dict.fromkeys(track_ids))
        out: dict[str, Track] = {}
        for start in range(0, len(ids), 500):
            chunk = ids[start : start + 500]
            marks = ",".join("?" * len(chunk))
            for row in self._query(f"SELECT * FROM tracks WHERE id IN ({marks})", chunk):
                out[row["id"]] = _track(row)
        return out

    _INSERT_TRACK = "INSERT INTO tracks(id, album_id, meta, created_at, updated_at) VALUES (?, ?, ?, ?, ?)"
    _MERGE_TRACK = "UPDATE tracks SET meta = ?, album_id = COALESCE(?, album_id), updated_at = ? WHERE id = ?"

    def _remember(self, c: sqlite3.Connection, track_id: str, meta: dict[str, Any], now: float) -> None:
        clean = {k: v for k, v in meta.items() if v is not None}
        row = c.execute("SELECT meta FROM tracks WHERE id = ?", (track_id,)).fetchone()
        if row is None:
            encoded = json.dumps(clean, ensure_ascii=False)
            c.execute(self._INSERT_TRACK, (track_id, clean.get("album_id"), encoded, now, now))
        else:
            merged = json.dumps({**json.loads(row["meta"] or "{}"), **clean}, ensure_ascii=False)
            c.execute(self._MERGE_TRACK, (merged, clean.get("album_id"), now, track_id))

    def remember_track(self, track_id: str, meta: dict[str, Any]) -> Track:
        """Insert or refresh the external metadata of a track; never touches its state."""
        with self.tx() as c:
            self._remember(c, track_id, meta, time.time())
        track = self.get_track(track_id)
        assert track is not None
        return track

    def remember_tracks(self, items: Iterable[tuple[str, dict[str, Any]]]) -> None:
        now = time.time()
        with self.tx() as c:
            for track_id, meta in items:
                self._remember(c, track_id, meta, now)

    _TRACK_COLUMNS = ("status", "file", "local_id", "source", "source_ref", "audio", "error", "attempts")

    def update_track(self, track_id: str, **fields: Any) -> None:
        unknown = set(fields) - set(self._TRACK_COLUMNS)
        if unknown:
            raise ValueError(f"unknown track fields: {sorted(unknown)}")
        if not fields:
            return
        values = [
            json.dumps(v, ensure_ascii=False) if k == "audio" and v is not None else v
            for k, v in fields.items()
        ]
        assignments = ", ".join(f"{k} = ?" for k in fields)
        with self.tx() as c:
            c.execute(
                f"UPDATE tracks SET {assignments}, updated_at = ? WHERE id = ?",
                (*values, time.time(), track_id),
            )

    def track_by_local_id(self, local_id: str) -> Track | None:
        rows = self._query("SELECT * FROM tracks WHERE local_id = ? LIMIT 1", (local_id,))
        return _track(rows[0]) if rows else None

    def tracks_by_local_ids(self, local_ids: Iterable[str]) -> dict[str, Track]:
        ids = list(dict.fromkeys(i for i in local_ids if i))
        out: dict[str, Track] = {}
        for start in range(0, len(ids), 500):
            chunk = ids[start : start + 500]
            marks = ",".join("?" * len(chunk))
            for row in self._query(f"SELECT * FROM tracks WHERE local_id IN ({marks})", chunk):
                out[row["local_id"]] = _track(row)
        return out

    def tracks_in_album(self, album_id: str) -> list[Track]:
        return [_track(r) for r in self._query("SELECT * FROM tracks WHERE album_id = ?", (album_id,))]

    def ready_track_by_artist_id(self, artist_id: str) -> Track | None:
        rows = self._query(
            "SELECT * FROM tracks WHERE status = 'ready' AND json_extract(meta, '$.artist_id') = ? LIMIT 1",
            (artist_id,),
        )
        return _track(rows[0]) if rows else None

    def tracks_with_status(self, *statuses: str) -> list[Track]:
        marks = ",".join("?" * len(statuses))
        return [_track(r) for r in self._query(f"SELECT * FROM tracks WHERE status IN ({marks})", statuses)]

    def ready_without_local_id(self, limit: int = 200) -> list[Track]:
        rows = self._query(
            "SELECT * FROM tracks WHERE status = 'ready' AND local_id IS NULL ORDER BY updated_at LIMIT ?",
            (limit,),
        )
        return [_track(r) for r in rows]

    def status_counts(self) -> dict[str, int]:
        return {
            r["status"]: r["n"] for r in self._query("SELECT status, COUNT(*) n FROM tracks GROUP BY status")
        }

    # --------------------------------------------------------------- aliases
    def add_alias(self, alias: str, track_id: str, how: str) -> bool:
        """Point ``alias`` at ``track_id``; an existing alias is never re-pointed."""
        if alias == track_id:
            return False
        with self.tx() as c:
            return (
                c.execute(
                    "INSERT OR IGNORE INTO aliases VALUES (?, ?, ?, ?)", (alias, track_id, how, time.time())
                ).rowcount
                > 0
            )

    def resolve_alias(self, alias: str) -> str | None:
        rows = self._query("SELECT track_id FROM aliases WHERE alias = ?", (alias,))
        return rows[0]["track_id"] if rows else None

    def aliases_of(self, track_id: str) -> list[tuple[str, str]]:
        rows = self._query(
            "SELECT alias, how FROM aliases WHERE track_id = ? ORDER BY created_at", (track_id,)
        )
        return [(r["alias"], r["how"]) for r in rows]

    def resolve_aliases(self, aliases: Iterable[str]) -> dict[str, str]:
        items = list(dict.fromkeys(aliases))
        out: dict[str, str] = {}
        for start in range(0, len(items), 500):
            chunk = items[start : start + 500]
            marks = ",".join("?" * len(chunk))
            for row in self._query(f"SELECT alias, track_id FROM aliases WHERE alias IN ({marks})", chunk):
                out[row["alias"]] = row["track_id"]
        return out

    def add_recording(self, key: str, track_id: str, duration: float) -> None:
        with self.tx() as c:
            c.execute("INSERT OR REPLACE INTO recordings VALUES (?, ?, ?)", (key, track_id, duration))

    def recordings(self, key: str) -> list[tuple[str, float]]:
        return [
            (r["track_id"], r["duration"])
            for r in self._query("SELECT track_id, duration FROM recordings WHERE key = ?", (key,))
        ]

    # ------------------------------------------------------------------ jobs
    def enqueue(
        self, track_id: str, kind: str, priority: int, reason: str | None = None, reset_attempts: bool = False
    ) -> bool:
        """Queue a job or raise the priority of an existing one.

        Returns True when the job is (re)queued and a worker should look at it.
        A running job is left alone; a failed or finished one is revived.
        """
        now = time.time()
        with self.tx() as c:
            row = c.execute("SELECT * FROM jobs WHERE track_id = ? AND kind = ?", (track_id, kind)).fetchone()
            if row is None:
                c.execute(
                    "INSERT INTO jobs(track_id, kind, priority, state, enqueued_at, reason) "
                    "VALUES (?, ?, ?, 'queued', ?, ?)",
                    (track_id, kind, priority, now, reason),
                )
                return True
            if row["state"] == "running":
                if priority < row["priority"]:
                    c.execute(
                        "UPDATE jobs SET priority = ? WHERE track_id = ? AND kind = ?",
                        (priority, track_id, kind),
                    )
                return False
            if row["state"] == "queued":
                changed = priority < row["priority"] or reset_attempts
                if changed:
                    c.execute(
                        "UPDATE jobs SET priority = MIN(priority, ?), not_before = CASE WHEN ? THEN 0 "
                        "ELSE not_before END, attempts = CASE WHEN ? THEN 0 ELSE attempts END "
                        "WHERE track_id = ? AND kind = ?",
                        (priority, reset_attempts, reset_attempts, track_id, kind),
                    )
                return changed
            c.execute(
                "UPDATE jobs SET state = 'queued', priority = ?, not_before = 0, enqueued_at = ?, "
                "reason = ?, error = NULL, attempts = CASE WHEN ? THEN 0 ELSE attempts END "
                "WHERE track_id = ? AND kind = ?",
                (priority, now, reason or row["reason"], reset_attempts, track_id, kind),
            )
            return True

    def get_job(self, track_id: str, kind: str) -> Job | None:
        rows = self._query("SELECT * FROM jobs WHERE track_id = ? AND kind = ?", (track_id, kind))
        return _job(rows[0]) if rows else None

    def jobs_in_state(self, state: str) -> list[Job]:
        return [
            _job(r)
            for r in self._query(
                "SELECT * FROM jobs WHERE state = ? ORDER BY priority, enqueued_at", (state,)
            )
        ]

    def claim_next_job(
        self, max_priority: int | None = None, min_priority: int | None = None, now: float | None = None
    ) -> Job | None:
        """Atomically move the best eligible queued job to ``running``."""
        now = time.time() if now is None else now
        clauses = ["state = 'queued'", "not_before <= ?"]
        args: list[Any] = [now]
        if max_priority is not None:
            clauses.append("priority <= ?")
            args.append(max_priority)
        if min_priority is not None:
            clauses.append("priority >= ?")
            args.append(min_priority)
        sql = f"SELECT * FROM jobs WHERE {' AND '.join(clauses)} ORDER BY priority, enqueued_at LIMIT 1"
        with self.tx() as c:
            row = c.execute(sql, args).fetchone()
            if row is None:
                return None
            c.execute(
                "UPDATE jobs SET state = 'running', attempts = attempts + 1 WHERE track_id = ? AND kind = ?",
                (row["track_id"], row["kind"]),
            )
            job = _job(row)
            job.state = "running"
            job.attempts += 1
            return job

    def finish_job(
        self, track_id: str, kind: str, state: str, error: str | None = None, retry_at: float | None = None
    ) -> None:
        with self.tx() as c:
            if retry_at is not None:
                c.execute(
                    "UPDATE jobs SET state = 'queued', not_before = ?, error = ? "
                    "WHERE track_id = ? AND kind = ?",
                    (retry_at, error, track_id, kind),
                )
            else:
                c.execute(
                    "UPDATE jobs SET state = ?, error = ? WHERE track_id = ? AND kind = ?",
                    (state, error, track_id, kind),
                )

    def requeue_running_jobs(self) -> int:
        """After a restart nothing is running; put interrupted jobs back in line."""
        with self.tx() as c:
            return c.execute(
                "UPDATE jobs SET state = 'queued', not_before = 0 WHERE state = 'running'"
            ).rowcount

    def next_retry_time(self) -> float | None:
        rows = self._query("SELECT MIN(not_before) t FROM jobs WHERE state = 'queued'")
        return rows[0]["t"] if rows and rows[0]["t"] is not None else None

    def job_counts(self) -> dict[str, int]:
        rows = self._query("SELECT kind || ':' || state k, COUNT(*) n FROM jobs GROUP BY kind, state")
        return {r["k"]: r["n"] for r in rows}

    # --------------------------------------------------------- user intents
    def add_pending_star(self, username: str, target: str) -> None:
        with self.tx() as c:
            c.execute("INSERT OR IGNORE INTO pending_stars VALUES (?, ?, ?)", (username, target, time.time()))

    def remove_pending_star(self, username: str, target: str) -> bool:
        with self.tx() as c:
            return (
                c.execute(
                    "DELETE FROM pending_stars WHERE username = ? AND target = ?", (username, target)
                ).rowcount
                > 0
            )

    def pending_stars(self, username: str | None = None) -> list[tuple[str, str, float]]:
        if username is None:
            rows = self._query("SELECT * FROM pending_stars ORDER BY created_at")
        else:
            rows = self._query(
                "SELECT * FROM pending_stars WHERE username = ? ORDER BY created_at", (username,)
            )
        return [(r["username"], r["target"], r["created_at"]) for r in rows]

    def add_pending_scrobble(self, username: str, track_id: str, when: int, submission: bool) -> None:
        with self.tx() as c:
            c.execute(
                "INSERT OR IGNORE INTO pending_scrobbles VALUES (?, ?, ?, ?)",
                (username, track_id, when, int(submission)),
            )

    def pending_scrobbles(self, username: str) -> list[tuple[str, int, bool]]:
        rows = self._query("SELECT * FROM pending_scrobbles WHERE username = ? ORDER BY time", (username,))
        return [(r["track_id"], r["time"], bool(r["submission"])) for r in rows]

    def remove_pending_scrobble(self, username: str, track_id: str, when: int, submission: bool) -> None:
        with self.tx() as c:
            c.execute(
                "DELETE FROM pending_scrobbles WHERE username = ? AND track_id = ? AND time = ? "
                "AND submission = ?",
                (username, track_id, when, int(submission)),
            )

    def add_pending_playlist_add(self, username: str, playlist_id: str, track_id: str) -> None:
        with self.tx() as c:
            c.execute(
                "INSERT OR IGNORE INTO pending_playlist_adds VALUES (?, ?, ?, ?)",
                (username, playlist_id, track_id, time.time()),
            )

    def pending_playlist_adds(self, username: str) -> list[tuple[str, str]]:
        rows = self._query(
            "SELECT * FROM pending_playlist_adds WHERE username = ? ORDER BY created_at", (username,)
        )
        return [(r["playlist_id"], r["track_id"]) for r in rows]

    def remove_pending_playlist_add(self, username: str, playlist_id: str, track_id: str) -> None:
        with self.tx() as c:
            c.execute(
                "DELETE FROM pending_playlist_adds WHERE username = ? AND playlist_id = ? AND track_id = ?",
                (username, playlist_id, track_id),
            )

    def users_with_pending_work(self) -> set[str]:
        rows = self._query(
            "SELECT username FROM pending_stars UNION SELECT username FROM pending_scrobbles "
            "UNION SELECT username FROM pending_playlist_adds"
        )
        return {r["username"] for r in rows}

    # ----------------------------------------------------------------- cache
    def cache_get(self, key: str) -> Any | None:
        rows = self._query("SELECT value, expires_at FROM cache WHERE key = ?", (key,))
        if not rows or rows[0]["expires_at"] < time.time():
            return None
        return json.loads(rows[0]["value"])

    def cache_set(self, key: str, value: Any, ttl: float) -> None:
        with self.tx() as c:
            c.execute(
                "INSERT INTO cache(key, value, expires_at) VALUES (?, ?, ?) ON CONFLICT(key) DO UPDATE SET "
                "value = excluded.value, expires_at = excluded.expires_at",
                (key, json.dumps(value, ensure_ascii=False), time.time() + ttl),
            )

    def cache_purge_expired(self) -> int:
        with self.tx() as c:
            return c.execute("DELETE FROM cache WHERE expires_at < ?", (time.time(),)).rowcount

    # --------------------------------------------------------------- imports
    def save_import(self, import_id: str, username: str, status: str, options: dict, rows: list) -> None:
        now = time.time()
        with self.tx() as c:
            c.execute(
                "INSERT INTO imports(id, username, status, options, rows, created_at, updated_at) "
                "VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(id) DO UPDATE SET status = excluded.status, "
                "rows = excluded.rows, updated_at = excluded.updated_at",
                (
                    import_id,
                    username,
                    status,
                    json.dumps(options, ensure_ascii=False),
                    json.dumps(rows, ensure_ascii=False),
                    now,
                    now,
                ),
            )

    def get_import(self, import_id: str) -> dict | None:
        rows = self._query("SELECT * FROM imports WHERE id = ?", (import_id,))
        if not rows:
            return None
        r = rows[0]
        return {
            "id": r["id"],
            "username": r["username"],
            "status": r["status"],
            "options": json.loads(r["options"]),
            "rows": json.loads(r["rows"]),
            "created_at": r["created_at"],
            "updated_at": r["updated_at"],
        }

    def list_imports(self, username: str, limit: int = 20) -> list[dict]:
        rows = self._query(
            "SELECT id FROM imports WHERE username = ? ORDER BY created_at DESC LIMIT ?", (username, limit)
        )
        return [imp for r in rows if (imp := self.get_import(r["id"])) is not None]

    def unfinished_imports(self) -> list[dict]:
        rows = self._query(
            "SELECT id FROM imports WHERE status IN ('queued', 'processing') ORDER BY created_at"
        )
        return [imp for r in rows if (imp := self.get_import(r["id"])) is not None]

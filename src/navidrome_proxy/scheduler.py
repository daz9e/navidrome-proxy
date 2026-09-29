"""The download queue: a priority lane and a fixed pool of background workers.

* Priority lane (P0 play, P1 like / next-track prefetch): every job starts
  immediately, it never waits behind background work.
* Background lane (P2 album likes, imports, upgrades): exactly
  ``background_workers`` jobs run at a time (5 by default).

One track has at most one job of each kind; asking again only raises its
priority. A play request for a track that a background worker is already
downloading simply waits for that download. Jobs survive restarts (SQLite).
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import threading
import time
from collections.abc import Awaitable, Callable

from .db import Job, Store, Track
from .sources import SourceError

log = logging.getLogger(__name__)

PLAY = 0
LIKE = 1
PREFETCH = 1
BACKGROUND = 2

BACKOFF = (15, 60, 300, 1200, 3600)
MAX_ATTEMPTS = len(BACKOFF)

Handler = Callable[[Job, threading.Event], Awaitable[Track | None]]


class Scheduler:
    def __init__(self, store: Store, background_workers: int = 5, priority_limit: int = 32):
        self.store = store
        self.background_workers = background_workers
        self.priority_limit = priority_limit
        self.handlers: dict[str, Handler] = {}
        self.on_done: list[Callable[[Job, Track], Awaitable[None]]] = []
        self._waiters: dict[str, list[asyncio.Future]] = {}
        self._priority_wake = asyncio.Event()
        self._background_wake = asyncio.Event()
        self._tasks: list[asyncio.Task] = []
        self._running: dict[tuple[str, str], tuple[asyncio.Task, threading.Event]] = {}
        self._priority_slots = asyncio.Semaphore(priority_limit)

    # ---------------------------------------------------------------- control
    def start(self) -> None:
        requeued = self.store.requeue_running_jobs()
        if requeued:
            log.info("requeued %d jobs interrupted by a restart", requeued)
        self._tasks.append(asyncio.create_task(self._priority_dispatcher(), name="priority-lane"))
        for n in range(self.background_workers):
            self._tasks.append(asyncio.create_task(self._background_worker(n), name=f"background-{n}"))

    async def stop(self) -> None:
        for _, cancel in self._running.values():
            cancel.set()
        for task in self._tasks + [t for t, _ in self._running.values()]:
            task.cancel()
        for task in self._tasks + [t for t, _ in self._running.values()]:
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await task
        self._tasks.clear()

    def request(
        self, track_id: str, priority: int, reason: str, kind: str = "download", fresh: bool = False
    ) -> bool:
        """Queue work for a track (idempotent). ``fresh`` gives a failed job a new attempt budget."""
        queued = self.store.enqueue(track_id, kind, priority, reason, reset_attempts=fresh)
        if queued or fresh:
            if kind == "download":
                track = self.store.get_track(track_id)
                if track is not None and track.status not in ("ready", "downloading"):
                    self.store.update_track(track_id, status="queued", error=None)
            self._wake(priority)
        return queued

    def _wake(self, priority: int) -> None:
        if priority <= PREFETCH:
            self._priority_wake.set()
        else:
            self._background_wake.set()

    async def wait(self, track_id: str, timeout: float) -> Track | None:
        """Wait until the download job of ``track_id`` finishes; the backing track or None."""
        track = self.store.get_track(track_id)
        if track is not None and track.is_ready:
            return track
        job = self.store.get_job(track_id, "download")
        if job is None or job.state in ("done", "failed"):
            return track if track is not None and track.is_ready else None
        future: asyncio.Future = asyncio.get_running_loop().create_future()
        self._waiters.setdefault(track_id, []).append(future)
        try:
            return await asyncio.wait_for(asyncio.shield(future), timeout)
        except TimeoutError:
            return None
        finally:
            waiters = self._waiters.get(track_id, [])
            if future in waiters:
                waiters.remove(future)

    def _resolve(self, track_id: str, result: Track | None) -> None:
        for future in self._waiters.pop(track_id, []):
            if not future.done():
                future.set_result(result)

    # ------------------------------------------------------------------ lanes
    def _next_wakeup(self) -> float:
        retry = self.store.next_retry_time()
        if retry is None:
            return 30.0
        return min(30.0, max(0.05, retry - time.time()))

    async def _priority_dispatcher(self) -> None:
        while True:
            job = self.store.claim_next_job(max_priority=PREFETCH)
            if job is None:
                self._priority_wake.clear()
                with contextlib.suppress(TimeoutError):
                    await asyncio.wait_for(self._priority_wake.wait(), self._next_wakeup())
                continue
            await self._priority_slots.acquire()
            task = asyncio.create_task(self._run_and_release(job), name=f"job-{job.track_id}")
            del task  # tracked in self._running by _run

    async def _run_and_release(self, job: Job) -> None:
        try:
            await self._run(job)
        finally:
            self._priority_slots.release()

    async def _background_worker(self, n: int) -> None:
        while True:
            job = self.store.claim_next_job(min_priority=BACKGROUND)
            if job is None:
                self._background_wake.clear()
                with contextlib.suppress(TimeoutError):
                    await asyncio.wait_for(self._background_wake.wait(), self._next_wakeup())
                continue
            await self._run(job)

    # -------------------------------------------------------------------- run
    async def _run(self, job: Job) -> None:
        handler = self.handlers.get(job.kind)
        key = (job.track_id, job.kind)
        cancel = threading.Event()
        current = asyncio.current_task()
        assert current is not None
        self._running[key] = (current, cancel)
        if job.kind == "download":
            self.store.update_track(job.track_id, status="downloading")
        started = time.monotonic()
        from .deezer import background

        background.set(job.priority >= BACKGROUND)
        try:
            if handler is None:
                raise SourceError(f"no handler for {job.kind}", transient=False)
            result = await handler(job, cancel)
        except asyncio.CancelledError:
            self.store.finish_job(job.track_id, job.kind, "queued", retry_at=time.time())
            raise
        except Exception as exc:
            self._failed(job, exc)
            return
        finally:
            self._running.pop(key, None)
        self.store.finish_job(job.track_id, job.kind, "done")
        log.info(
            "%s %s done in %.1fs (P%d, %s)",
            job.kind,
            job.track_id,
            time.monotonic() - started,
            job.priority,
            job.reason,
        )
        if job.kind == "download":
            self._resolve(job.track_id, result)
        if result is not None:
            for hook in self.on_done:
                try:
                    await hook(job, result)
                except Exception:
                    log.exception("post-job hook failed for %s", job.track_id)

    def _failed(self, job: Job, exc: Exception) -> None:
        transient = getattr(exc, "transient", True)
        message = str(exc)[-800:] or exc.__class__.__name__
        if transient and job.attempts < MAX_ATTEMPTS:
            delay = BACKOFF[min(job.attempts - 1, len(BACKOFF) - 1)]
            log.warning(
                "%s %s failed (attempt %d), retry in %ds: %s",
                job.kind,
                job.track_id,
                job.attempts,
                delay,
                message,
            )
            self.store.finish_job(
                job.track_id, job.kind, "queued", error=message, retry_at=time.time() + delay
            )
            self._wake(job.priority)  # re-arm the lane's timer for the retry
            if job.kind == "download":
                self.store.update_track(job.track_id, status="queued", error=message)
                # A waiting stream should not hang for minutes on a retry.
                self._resolve(job.track_id, None)
            return
        log.warning("%s %s failed permanently: %s", job.kind, job.track_id, message)
        self.store.finish_job(job.track_id, job.kind, "failed", error=message)
        if job.kind == "download":
            track = self.store.get_track(job.track_id)
            if track is not None and not track.is_ready:
                self.store.update_track(job.track_id, status="failed", error=message)
            self._resolve(job.track_id, None)

    def running(self) -> list[tuple[str, str]]:
        return list(self._running)

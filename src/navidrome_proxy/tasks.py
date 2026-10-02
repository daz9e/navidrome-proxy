"""Fire-and-forget work that must not be garbage-collected mid-flight."""

from __future__ import annotations

import asyncio
import contextlib
from collections.abc import Coroutine
from typing import Any


class Background:
    """Keeps a strong reference to each spawned task until it finishes.

    The event loop only holds weak references to tasks, so a bare
    ``asyncio.create_task(...)`` whose result is dropped can vanish before it runs.
    """

    def __init__(self) -> None:
        self._tasks: set[asyncio.Task] = set()

    def spawn(self, coro: Coroutine[Any, Any, Any], name: str | None = None) -> asyncio.Task:
        task = asyncio.create_task(coro, name=name)
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return task

    async def cancel(self) -> None:
        """Cancel everything still running (at shutdown) and wait for it to unwind."""
        tasks = list(self._tasks)
        for task in tasks:
            task.cancel()
        for task in tasks:
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await task

    def __len__(self) -> int:
        return len(self._tasks)

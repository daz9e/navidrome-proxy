import asyncio

from navidrome_proxy.tasks import Background


async def test_spawned_tasks_are_held_until_done_and_cancelled_at_shutdown():
    background = Background()
    done = asyncio.Event()
    started = asyncio.Event()

    async def quick():
        done.set()

    async def forever():
        started.set()
        await asyncio.sleep(3600)

    background.spawn(quick())
    slow = background.spawn(forever())
    await done.wait()
    await started.wait()
    await asyncio.sleep(0)
    assert len(background) == 1  # finished tasks drop out on their own

    await background.cancel()
    assert slow.cancelled()
    assert len(background) == 0

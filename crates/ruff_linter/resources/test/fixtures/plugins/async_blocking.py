import asyncio
import time

from acme import db


async def handler(request):
    rows = db.query("SELECT 1")
    time.sleep(1)

    def in_thread():
        return db.query("SELECT 2")  # OK: runs in a thread

    await asyncio.to_thread(in_thread)
    await asyncio.sleep(1)  # OK


def sync_handler():
    db.query("SELECT 3")  # OK


@cache(time.sleep(0))  # OK: runs when the module loads
async def decorated(delay=db.query("SELECT 4")):  # OK
    pass

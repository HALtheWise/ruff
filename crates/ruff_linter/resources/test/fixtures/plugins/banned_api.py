import pickle
import subprocess
import yaml as y
from datetime import datetime
from pickle import loads as unpickle


def loads(data):
    return data


pickle.loads(b"")
unpickle(b"")
y.load("")
y.safe_load("")  # OK
datetime.utcnow()
datetime.now()  # OK
subprocess.run(["ls"])
loads(b"")  # OK: a local function

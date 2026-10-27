import os
from os import environ, getenv as env

debug = os.environ.get("DEBUG")
token = os.environ["TOKEN"]
home = environ["HOME"]
port = env("PORT")
cwd = os.getcwd()  # OK

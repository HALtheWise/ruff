import os, drop_module
from os import path, drop_name
import drop_whole


def f():
    remove_me()


try:
    pass
except ValueError:
    remove_me()

match x:
    case 1:
        remove_me()

if a:
    pass
elif b:
    remove_me()

a = 1; remove_me(); b = 2

g(1, drop_arg, 2)
g(1, drop_kw=1)
t = (drop_elt, 1)
t = 1, drop_elt
s = {drop_elt}

g(1, pair_a=1, pair_b=2)


def h():
    before()
    after()


if c: inline()

# Deleting a key from a dictionary would break it, so the plugin fails (and isn't called for
# the rest of the file).
d = {drop_key: 1}

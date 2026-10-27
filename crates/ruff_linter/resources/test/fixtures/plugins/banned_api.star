# Project-specific banned APIs, each with the reason and the approved alternative.
#
# Unlike a text search, matching is semantic: `from pickle import loads as l; l(data)` is caught,
# and so is `import yaml as y; y.load(...)`, while a local function named `loads` is not.
#
#     [tool.ruff.lint.plugins.banned-api]
#     path = "lint/banned_api.star"
#     options = { banned = { "requests.get" = "Use `acme.http.get`, which sets timeouts and retries." } }

BANNED = option(
    "banned",
    default = {
        "pickle.loads": "Unpickling untrusted data can execute arbitrary code; use `acme.serde.loads`.",
        "yaml.load": "Use `yaml.safe_load`, which can't construct arbitrary objects.",
        "datetime.datetime.utcnow": "`utcnow()` returns a naive datetime; use `datetime.now(UTC)`.",
        "subprocess.*": "Use `acme.proc.run`, which logs and enforces timeouts.",
    },
    help = "A map from qualified names (or `module.*` patterns) to the reason they're banned.",
)

BANNED_API = rule("BAN001", "banned-reference")

def reason_for(qualname):
    if qualname in BANNED:
        return BANNED[qualname]
    for pattern, reason in BANNED.items():
        if pattern.endswith(".*") and qualname.startswith(pattern[:-1]):
            return reason
    return None

# `references(...)` matches qualified names natively, so the callback only runs for banned names.
references(*BANNED.keys()).foreach(
    lambda name: BANNED_API.report(name, "`{}` is banned: {}".format(name.qualname, reason_for(name.qualname))),
)

# A codemod for an internal API change: `acme.http.fetch(url, timeout_ms)` became
# `acme.http.get(url, timeout)`, with the timeout in seconds.

DEPRECATED_FETCH = rule("MIG001", "deprecated-fetch")

def check(call):
    message = "`acme.http.fetch` is deprecated; use `acme.http.get`"
    if type(call.func()) != "Attribute":
        # Fixing `from acme.http import fetch` would also need a change to the import.
        DEPRECATED_FETCH.report(call.func(), message)
        return

    edits = [replace(call.func().span("attr"), "get")]
    timeout = call.keyword("timeout_ms")
    if timeout:
        milliseconds = timeout.value().literal()
        if type(milliseconds) != "int":
            # A timeout in a variable can't be converted automatically.
            DEPRECATED_FETCH.report(call.func(), message)
            return
        edits.append(replace(timeout, "timeout={}".format(milliseconds / 1000)))

    DEPRECATED_FETCH.report(call.func(), message, fix = edits, fix_title = "Replace with `acme.http.get`")

calls("acme.http.fetch").foreach(check)

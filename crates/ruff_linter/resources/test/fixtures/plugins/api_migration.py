from acme import http

http.fetch("https://example.com", timeout_ms=1500)
http.fetch(
    "https://example.com",
    timeout_ms=250,
    retries=3,
)

delay = 100
http.fetch("https://example.com", timeout_ms=delay)  # No fix: the timeout isn't a literal

# Structured log events are named with snake_case string literals and carry their data as
# keyword arguments, so that they can be searched and aggregated:
#
#     log.info("payment_failed", order_id=order.id)
#
# Loggers are usually module-level variables (`log = structlog.get_logger()`) rather than
# imports, so this matches calls by their names instead of by qualified name.

LOGGERS = option("loggers", default = ["log", "logger"], help = "Names of logger variables.")
METHODS = ["debug", "info", "warning", "error", "exception", "critical"]

UNSTRUCTURED_EVENT = rule("SLOG001", "unstructured-log-event")

def check(method):
    call = method.parent()
    if type(call) != "Call" or call.func() != method:
        return
    if type(method.value()) != "Name" or method.value().id not in LOGGERS:
        return
    event = call.argument(0)
    name = event.literal() if event else None
    if type(name) != "string" or not re.fullmatch(r"[a-z][a-z0-9_]*", name):
        UNSTRUCTURED_EVENT.report(
            event or call,
            "Log events should be snake_case string literals, with data passed as keyword arguments",
        )

# Selecting attributes by name natively, rather than every call, keeps the callback rare.
nodes("Attribute").filter_name("^({})$".format("|".join(METHODS))).foreach(check)

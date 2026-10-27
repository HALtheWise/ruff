# Blocking calls stall the event loop when they're made from a coroutine. This extends ruff's
# `ASYNC` rules with a project's own blocking APIs.

BLOCKING = option(
    "blocking",
    default = ["acme.db.query", "acme.db.execute", "requests.*", "time.sleep"],
    help = "Qualified names (or `module.*` patterns) of functions that block.",
)

BLOCKING_CALL = rule("ASY001", "blocking-call-in-coroutine")

def check(call):
    # The innermost enclosing function decides: a regular function defined in a coroutine (and
    # run in a thread, say) may block.
    function = call.enclosing("FunctionDef", "AsyncFunctionDef", "Lambda")
    # Decorators and default values run where the function is defined, not in the coroutine.
    # They're inside the function's statement, but not inside a statement of its body.
    if function and type(function) == "AsyncFunctionDef" and call.enclosing("stmt") != function:
        BLOCKING_CALL.report(
            call.func(),
            "`{}` blocks the event loop when called from the coroutine `{}`".format(call.func().text, function.name),
        )

calls(*BLOCKING).foreach(check)

# Checks of what callbacks can do.

DESCRIBE_CALL = rule("FUN001", "describe-call")
LAST_ARGUMENT = rule("FUN002", "last-argument")
FAILING_CALLBACK = rule("FUN003", "failing-callback")
SHOUTING = rule("FUN004", "shouting-string")
REPEATED_CALL = rule("FUN005", "repeated-call")

# Nodes have the fields of Python's `ast`: fields that hold plain data are attributes, like
# `keyword.arg`, and fields that hold nodes are methods, like `call.func()`. They also have the
# attributes `text`, `line`, and `qualname`, and `type(node)` is the node's kind, like `"Call"`.
# A callback can register checks scoped to the node it receives, keep state in its variables, and
# report once ruff has visited everything inside the node.
def describe(call):
    nested = []
    call.calls().foreach(nested.append)

    def report():
        DESCRIBE_CALL.report(call, "{} on line {}: func={} args={} keywords={} nested_calls={}".format(
            type(call),
            call.line,
            call.func().text,
            [arg.value for arg in call.args() if type(arg) == "Constant"],
            [keyword.arg for keyword in call.keywords()],
            [nested_call.func().text for nested_call in nested],
        ))

    call.on_exit(report)

calls("acme.trace").foreach(describe)

# A diagnostic can be reported on any node.
calls("acme.trace").foreach(lambda call: call.args() and LAST_ARGUMENT.report(
    call.args()[-1],
    "The last argument is `{}`".format(call.args()[-1].text),
))

# A callback that fails reports a warning, and isn't called again for the rest of the file.
calls("acme.trace").foreach(lambda call: call.body())

# `re` provides regular expressions.
def check_string(constant):
    if type(constant.value) == "string" and re.fullmatch(r"[A-Z ]{3,}", constant.value):
        SHOUTING.report(constant, "Don't shout: `{}`".format(constant.value.lower()))

nodes("Constant").foreach(check_string)

# Nodes can be dictionary keys, and state can span the whole file.
def check_module(module):
    calls_by_text = {}

    def record(call):
        calls_by_text.setdefault(call.text, []).append(call)

    def report():
        for text, repeated in calls_by_text.items():
            for call in repeated[1:]:
                REPEATED_CALL.report(call, "`{}` repeats the call on line {}".format(text, repeated[0].line))

    module.calls("acme.trace").foreach(record)
    module.on_exit(report)

modules().foreach(check_module)

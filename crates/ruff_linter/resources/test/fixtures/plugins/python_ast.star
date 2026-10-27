# Checks that plugins see ruff's AST as Python's `ast` module describes it, and that edits
# produce valid code.

ELIF = rule("AST001", "elif-is-nested-if")
DEBUG_KEYWORD = rule("AST002", "debug-keyword")
UNANNOTATED_PARAMETER = rule("AST003", "unannotated-parameter")
BREAKPOINT = rule("AST004", "breakpoint-call")
UNTRACED_HANDLER = rule("AST005", "untraced-handler")
MAGIC_RETRY_COUNT = rule("AST006", "magic-retry-count")

# `elif` is an `If` in the `orelse` of the preceding `If`.
def check_if(node):
    if node.orelse():
        ELIF.report(node.test(), "`if {}` has an `elif` or `else`: {}".format(
            node.test().text,
            ", ".join([stmt.text for stmt in node.orelse()]),
        ))

nodes("If").foreach(check_if)

# Keyword arguments are nodes of their own. Deleting one removes its comma too.
nodes("keyword").filter_name("^debug$").foreach(lambda keyword: DEBUG_KEYWORD.report(
    keyword,
    "Remove `debug={}`".format(keyword.value().text),
    fix = delete(keyword),
    fix_title = "Remove the argument",
))

# Parameters are `arg` nodes, with a `default` (which Python's `ast` keeps in `arguments`).
def check_parameter(parameter):
    if parameter.annotation() == None and parameter.arg not in ["self", "cls"]:
        UNANNOTATED_PARAMETER.report(parameter, "`{}` has no annotation".format(parameter.arg))

nodes("arg").foreach(check_parameter)

# Deleting a statement deletes its line, or replaces it with `pass` if it's alone in its block.
def check_breakpoint(call):
    statement = call.parent()
    if type(statement) == "Expr":
        BREAKPOINT.report(statement, "Remove `breakpoint()`", fix = delete(statement), fix_title = "Remove the call")

calls("builtins.breakpoint").foreach(check_breakpoint)

# Inserting before a statement inserts a line, with the statement's indentation.
def check_handler(function):
    if not any([decorator.qualname == "acme.trace" for decorator in function.decorator_list()]):
        UNTRACED_HANDLER.report(
            function.span("name"),
            "Handlers should be traced",
            fix = insert_before(function, "@trace"),
            fix_title = "Add `@trace`",
        )

nodes("FunctionDef").filter_name("^handle_").foreach(check_handler)

# Literals, following `ast.literal_eval`.
def check_retries(keyword):
    count = keyword.value().literal()
    if type(count) == "int" and (count > 3 or count < 0):
        MAGIC_RETRY_COUNT.report(keyword, "Name this retry count: {}".format(count))

nodes("keyword").filter_name("^retries$").foreach(check_retries)

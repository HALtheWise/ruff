# Habits of generated code that reviewers keep asking to remove.

NARRATING_COMMENT = rule("SLOP001", "narrating-comment")
PLACEHOLDER_FUNCTION = rule("SLOP002", "placeholder-function")

comments(
    r"^#\s*(Step \d+:|This (function|method|line|code|block|loop) |Here we |Now we |We (now |then )?(create|define|call|loop|check|return|import|initialize))",
).foreach(lambda comment: NARRATING_COMMENT.report(
    comment,
    "This comment narrates the code; explain why instead, or remove it",
))

# Abstract methods, overloads, and protocol members are placeholders by design.
DESIGNED_DECORATORS = ["abc.abstractmethod", "typing.overload", "typing_extensions.overload"]
DESIGNED_BASES = ["typing.Protocol", "typing_extensions.Protocol", "abc.ABC"]

def is_placeholder(stmt):
    """Whether a statement does nothing: a docstring or `...`, `pass`, or `raise NotImplementedError`."""
    if type(stmt) == "Pass":
        return True
    if type(stmt) == "Expr":
        return type(stmt.value()) == "Constant"
    if type(stmt) == "Raise":
        return stmt.exc() != None and stmt.exc().text.startswith("NotImplementedError")
    return False

def check_function(function):
    if not all([is_placeholder(stmt) for stmt in function.body()]):
        return
    if any([decorator.qualname in DESIGNED_DECORATORS for decorator in function.decorator_list()]):
        return
    cls = function.enclosing("ClassDef")
    if cls and any([base.qualname in DESIGNED_BASES for base in cls.bases()]):
        return
    PLACEHOLDER_FUNCTION.report(
        function.span("name"),
        "`{}` is a placeholder; implement it or remove it".format(function.name),
    )

nodes("FunctionDef", "AsyncFunctionDef").foreach(check_function)

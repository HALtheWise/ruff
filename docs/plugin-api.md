# Plugin API reference

> **Preview:** Plugins are a [preview](https://docs.astral.sh/ruff/preview/) feature, and this API
> may change.

This page describes everything that [plugins](plugins.md) can do.

## Enabling plugins

List plugins under `lint.plugins`, mapping a name of your choice to the plugin's file, or to a table with
the file's `path` and the plugin's `options`. Paths are relative to the directory of the configuration
file that lists the plugin, which is also the root that the plugin's [path filters](#selectors) and
`module.path` are relative to:

```toml
# ruff.toml (or, in pyproject.toml, under [tool.ruff.lint] and [tool.ruff.lint.plugins])
[lint]
preview = true

[lint.plugins]
http = "lint/http.star"
layers = { path = "lint/layers.star", options = { layers = ["app.api", "app.domain"] } }
```

All rules of a listed plugin are enabled. Their diagnostics can be suppressed with `# noqa: HTTP001`,
`# ruff: noqa: HTTP001`, and `# ruff: ignore[HTTP001]` comments, which [`unused-noqa`](https://docs.astral.sh/ruff/rules/unused-noqa/)
checks like any others, and their fixes are applied by `ruff check --fix`.

Plugins are part of the configuration: a configuration that [`extend`s](https://docs.astral.sh/ruff/settings/#extend) another
inherits its plugins, and a plugin listed under a name that's already taken replaces the inherited one.
Paths given on the command line, with `--config "lint.plugins.http = 'lint/http.star'"`, are relative to
the current directory.

If a plugin fails to load, `ruff check` fails with an error that points into the plugin, and `ruff format`,
which doesn't use plugins, works as usual. An editor using Ruff's language server shows the error and lints
without the plugins until it's fixed.

## Writing plugins

### Starlark for Python programmers

Starlark looks like Python, but it's smaller, and it's deterministic: a plugin can't read files, use the
network, or tell the time. Plugins use Starlark's extended dialect, which allows nested functions,
lambdas, and `for` and `if` at the top level. The differences that Python programmers run into first:

| In Python                        | In a plugin                                                                                                  |
| -------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `import`, `class`, `try`, `with` | Not available.                                                                                               |
| `while`                          | Not available; loop over a `range(...)` or a list.                                                           |
| `x is None`                      | `x == None`.                                                                                                 |
| `f"{x.y}"`                       | f-strings take variable names only: `f"{name}"`. Use `"{}".format(x.y)`.                                     |
| `any(x for x in xs)`             | Generator expressions aren't available; use a list: `any([x for x in xs])`.                                  |
| `{1, 2}`                         | Use `set([1, 2])`.                                                                                           |
| `for char in text`               | Strings aren't iterable; use `text.elems()`.                                                                 |
| `nonlocal x`                     | Not available; keep state in a list or dictionary (see [State and scoped checks](#state-and-scoped-checks)). |
| `print(...)`                     | Prints to standard error, for debugging.                                                                     |

Starlark also has `json.encode` and `json.decode`, and `struct(...)` for records.

### Rules

`rule(code, name)` declares a rule, and returns an object that callbacks report diagnostics through:

```python
LAUNCHED_FLAG = rule("FLAG001", "launched-feature-flag")

LAUNCHED_FLAG.report(call, "The feature flag `new-checkout` has launched")
```

Codes are uppercase letters followed by digits, and can't be codes of Ruff's own rules. Names are
kebab-case. Declaring rules up front lets Ruff check for clashes and suppress diagnostics with `noqa`, and
because Starlark resolves names when it loads a file, a misspelled rule is an error when the plugin loads,
not when a callback finally reports.

### Selectors and callbacks

A selector picks the nodes that a callback sees, and `.foreach(callback)` registers the callback:

```python
nodes("FunctionDef", "AsyncFunctionDef").filter_path("tests/**").filter_name("^test_").foreach(check_test)
```

Selectors start from `nodes(*kinds)`, `calls(*names)`, `references(*names)`, `imports(*names)`,
`comments(*patterns)`, or `modules()`, and are narrowed with filters, which Ruff evaluates natively:
`.filter_path(*globs)` and `.exclude_path(*globs)` (checked once per file), `.filter_name(pattern)`, and
`.filter_qualname(*names)`. Selectors combine with `|`, as in `nodes("Assert") | calls("pytest.raises")`.
Everything else is plain Starlark, in the callback.

Selectors created at the top level of a plugin apply to every file. In a callback, select the nodes inside
the node or module that the callback received, as in `function.calls(...)` (see
[State and scoped checks](#state-and-scoped-checks)).

Qualified names are resolved through imports and aliases, as Ruff's own rules resolve them: `requests.get`
matches `requests.get(...)`, and `fetch(...)` after `from requests import get as fetch`, but not a
local function called `get`. Builtins are qualified as `builtins.print`, and a name ending in `.*` matches
everything in a module, as in `calls("subprocess.*")`.

### Nodes

Callbacks receive nodes of the syntax tree, as described by Python's
[`ast`](https://docs.python.org/3/library/ast.html) module: a call is a `Call` node, with the fields
`func`, `args`, and `keywords`, a string or number is a `Constant`, an `elif` is an `If` in the `orelse`
of the preceding `If`, and so on (see [Node kinds](#node-kinds)). Each kind is a type of its own, so
`type(node)` is the node's kind, like `"Call"`, as `type(node).__name__` is in Python. Selectors and
methods like `nodes(...)` and `enclosing(...)` take the same names.

Fields follow one rule: fields that hold plain data, like names, operators, and numbers, are
**attributes**, and fields that hold other nodes are **methods**, because finding those nodes does
work. So `function.name` is a string and `function.body()` is a list of statements, `binop.op` is
`"Add"` and `binop.left()` is a node, and `call.func().attr` is the method name in a call like
`client.get(url)`.

Nodes also have these attributes:

| Attribute  | Value                                                                                                                                                                                                                                                                                                |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `text`     | The node's source code.                                                                                                                                                                                                                                                                              |
| `line`     | The line the node starts on.                                                                                                                                                                                                                                                                         |
| `qualname` | For a name, attribute, or call, the qualified name it refers to (for a call, that of the callee), like `"requests.get"`. `None` for other expressions, or names that refer to local variables. For an `alias`, the qualified name of what's imported, with relative imports resolved (in a package). |

`qualname` is resolved like Ruff's own rules resolve names: with the bindings that are visible where Ruff
is in the file when the node's value is created. For the nodes that Ruff passes to a callback, that's
where the node is. A node that a callback reaches through methods, like a call in `function.body()`, is
resolved where the callback's own node is, before Ruff has visited the function's body. To resolve the
names inside a node, select them with a scoped selector like `function.calls(...)`, whose callbacks run
as Ruff visits each call.

And these methods, besides those of their fields:

| Method                                                                              | Value                                                                                                                                                                                                                                     |
| ----------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `.parent()`                                                                         | The node that contains this one (such as the `keyword` around the value of a keyword argument), or `None` for a statement at the top level of the module, or a comment.                                                                   |
| `.enclosing(*kinds)`                                                                | The innermost node of one of `kinds` that contains this one, or `None`.                                                                                                                                                                   |
| `.span(field)`                                                                      | The position of a name field, like the `name` of a function or the `attr` of an attribute, to report on or edit.                                                                                                                          |
| `.literal()`                                                                        | For an expression made of literals, its value: a string, a number, `True`, `False`, or `None`, or a list of those for a list or tuple (also for a negative number, like `-1`). `None` for anything else, including dictionaries and sets. |
| `.argument(position)`, `.argument(name)`, `.argument(name, position)`               | For a call, the expression passed for an argument by position, by keyword, or either way, or `None`.                                                                                                                                      |
| `.keyword(name)`                                                                    | For a call, the `keyword` node of a keyword argument, or `None`.                                                                                                                                                                          |
| `.nodes(...)`, `.calls(...)`, `.references(...)`, `.imports(...)`, `.comments(...)` | Selectors of the nodes inside this one (see [State and scoped checks](#state-and-scoped-checks)).                                                                                                                                         |
| `.on_exit(callback)`                                                                | Calls `callback` once everything inside the node has been checked.                                                                                                                                                                        |

Nodes compare equal when they're the same node, and can be used as dictionary keys.

A `Constant`'s `value` is a string, a number, `True`, `False`, or `None`. Bytes are decoded as strings, and
`...`, complex numbers, and integers that don't fit in 64 bits are `None`; use the node's `text` to tell
them apart.

### State and scoped checks

Ruff's own rules often gather information from many nodes before they decide what to report. Plugins do
the same with the variables of their callbacks. A callback can register checks that only see the nodes
inside the node it received, and an `on_exit` callback that runs once those checks have seen everything:

```python
NO_ASSERTION = rule("TST002", "test-without-assertion")

def check_test(function):
    assertions = []
    function.nodes("Assert").foreach(assertions.append)
    function.calls("pytest.raises").foreach(assertions.append)
    function.on_exit(lambda: assertions or NO_ASSERTION.report(
        function.span("name"),
        "`{}` doesn't assert anything".format(function.name),
    ))

nodes("FunctionDef").filter_name("^test_").foreach(check_test)
```

State that spans a whole file lives in a `modules()` callback, which runs once per file, before Ruff
visits any of the file's nodes:

```python
DUPLICATE_ROUTE = rule("WEB001", "duplicate-route")

def check_module(module):
    seen = {}

    def check(call):
        route = call.argument("route", 0)
        if route == None or type(route.literal()) != "string":
            return
        if route.literal() in seen:
            DUPLICATE_ROUTE.report(route, "`{}` is already routed on line {}".format(route.literal(), seen[route.literal()].line))
        else:
            seen[route.literal()] = call

    module.calls("django.urls.path").foreach(check)

modules().filter_path("**/urls.py").foreach(check_module)
```

Starlark's closures can't reassign the variables of their enclosing function (there's no `nonlocal`), but
they can change the lists and dictionaries those variables hold. Values defined at the top level of a
plugin can't change once it has loaded, so state can't leak from one file to the next.

### Fixes

`report(...)` takes an optional `fix`: an edit, or a list of edits, which Ruff applies together.

| Edit                          | Effect                                                                                                                       |
| ----------------------------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `replace(target, text)`       | Replaces `target` with `text`.                                                                                               |
| `delete(target)`              | Deletes `target`, keeping the code valid (see below).                                                                        |
| `insert_before(target, text)` | Inserts `text` before `target`. Before a statement, `text` becomes lines of their own, with the statement's indentation.     |
| `insert_after(target, text)`  | Inserts `text` after `target`, with the same treatment of statements (after any comment at the end of the statement's line). |

`target` is a node, or a span like `function.span("name")`. Fixes are [safe](https://docs.astral.sh/ruff/linter/#fix-safety) unless
the rule is declared with `fix_safety = "unsafe"` (or `"display-only"`), and `fix_title` describes the fix
(it's ignored when `fix` is `None`):

```python
LAUNCHED_FLAG.report(call, message, fix = replace(call, "True"), fix_title = "Replace with `True`")
```

Edits know Python's syntax, as Ruff's own fixes do:

- Deleting a statement deletes its line, or its `;` if it shares the line, and replaces it with `pass` if
    it's the only statement in its block (including `except` clauses, `match` cases, and `elif` and `else`
    clauses). Fixes that delete statements from the same block are applied one at a time, so that together
    they can't empty it.
- Deleting an argument, an element of a list, set, or tuple, an imported name, a `with` item, or a
    parameter deletes its comma too. A tuple that's left with one element keeps a comma (`(a, b)` becomes
    `(b,)`), and an emptied set becomes `set()`.
- Deleting the only name of an import, or the value of an expression statement (like the call in
    `breakpoint()`), deletes the statement.
- Deleting a decorator, or an `except` clause or `match` case that isn't the only one, deletes its lines.
- Inserting next to a statement that shares its line with another one uses `;`.

Deleting a node that can't be removed without breaking the code, like the test of an `if` or a key of a
dictionary, is an error (see [Errors](#errors)): replace the code around it instead. So is a fix whose
edits overlap, except deletions, which are merged (so deleting two adjacent arguments works). Edits that
don't change anything are dropped.

After applying fixes, Ruff lints the file again, so a fix must make its diagnostic go away. If fixes keep
applying, or one produces invalid code, Ruff stops (and reverts the file, for invalid code) and names the
plugin responsible.

### Options

Plugins declare options with `option(name, default, help = ...)`, which returns the value from the
plugin's `options` table in the configuration, or `default`. Ruff reports options that a plugin doesn't
declare, and values whose type doesn't match the default's.

```python
LAYERS = option("layers", default = [], help = "Module prefixes, from the highest layer to the lowest.")
```

### Regular expressions

Starlark doesn't have regular expressions, so plugins get an `re` module modeled on Python's, with
`re.search`, `re.match`, `re.fullmatch`, `re.findall`, `re.sub`, `re.split`, and `re.escape`. Patterns
use the syntax of Rust's [`regex`](https://docs.rs/regex) crate, which is close to Python's but doesn't
support look-around or backreferences. In replacements, groups are `$1` or `${name}`.

### Errors

Mistakes that Ruff can catch when it loads a plugin, like an unknown node kind or an invalid pattern, stop
Ruff with an error that points into the plugin. If a callback fails while Ruff lints a file, Ruff reports a
`plugin-error` diagnostic at the node that the callback received, with a traceback into the plugin, and
stops calling that function for the rest of the file. Like a syntax error, a `plugin-error` fails the run
and can't be suppressed: only fixing the plugin removes it. Ruff also suggests fixes for common mistakes:

```text
plugin-error: Plugin `http` failed: Object of type `function` has no attribute `attr` (at lint/http.star:6)
 --> app.py:4:1
...
help: Fields that hold nodes are methods, so call them: `call.func().attr`, not `call.func.attr`
```

## Reference

### Selectors

| Function              | Selects                                                                                                                                                                                                         |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `nodes(*kinds)`       | Nodes of the given [kinds](#node-kinds), like `nodes("FunctionDef", "AsyncFunctionDef")`. `"stmt"` and `"expr"` stand for all statements and all expressions. Comments are selected with `comments(...)`.       |
| `calls(*names)`       | Calls, optionally only of functions with the given qualified names.                                                                                                                                             |
| `references(*names)`  | Names and attributes, optionally only those that refer to the given qualified names.                                                                                                                            |
| `imports(*names)`     | One `alias` node per imported name, optionally only those whose `qualname` is one of the given names.                                                                                                           |
| `comments(*patterns)` | One `Comment` node per comment, whose `text` includes the `#`, optionally only those that match one of the regular expressions.                                                                                 |
| `modules()`           | The file, as a `Module` with the attributes `path` (relative to the plugin's root) and `name` (its dotted name, like `app.api.views`, the package's name for an `__init__.py`, or `None` outside of a package). |

| Method                     | Description                                                                                                                                                                                                                                                                                                                           |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `.filter_path(*globs)`     | Keeps the nodes of files whose path, relative to the plugin's root and with `/` separators, matches one of the glob patterns. `*` matches across `/` too.                                                                                                                                                                             |
| `.exclude_path(*globs)`    | Drops the nodes of files whose path matches one of the glob patterns.                                                                                                                                                                                                                                                                 |
| `.filter_name(pattern)`    | Keeps the nodes whose name matches the regular expression anywhere (anchor it with `^` and `$` to match the whole name): the `name` of a definition or exception handler, the `id` of a `Name`, the `attr` of an `Attribute`, the `arg` of a parameter or keyword argument, the `name` of an `alias`, or the dotted name of a module. |
| `.filter_qualname(*names)` | Keeps the nodes whose `qualname` is one of the given names, or modules whose dotted name is.                                                                                                                                                                                                                                          |
| `a \| b`                   | Selects the nodes of either selector.                                                                                                                                                                                                                                                                                                 |
| `.foreach(callback)`       | Calls `callback` with each selected node as Ruff visits it.                                                                                                                                                                                                                                                                           |

Selectors created at the top level of a plugin apply to every file. Selectors created from a module or
node apply to the rest of that file, or to the nodes inside that node, and can only be registered while
Ruff visits the file's nodes: not in `on_exit` callbacks, or in callbacks of `comments(...)`, which run
after Ruff has visited the nodes. A node's comments include a comment at the end of its last line.

### Rules and other values

| Function or attribute                                           | Description                                                                     |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `rule(code, name, *, fix_safety = "safe", url = None)`          | Declares a rule. `url` links to the rule's documentation, in Ruff's output.     |
| `rule.report(target, message, *, fix = None, fix_title = None)` | Reports a diagnostic on a node, a span, or a module (at the start of the file). |
| `rule.code`, `rule.name`                                        | The rule's code and name.                                                       |
| `node.span(field)`                                              | A span, with the attributes `text` and `line`.                                  |
| `option(name, default = None, help = None)`                     | The value of one of the plugin's options (see [Options](#options)).             |

### Order of callbacks

Ruff visits each node before the nodes inside it, so a check registered by a callback sees every node
inside the node the callback received. A node's `on_exit` callbacks run after the callbacks of every node
inside it (and after the `on_exit` callbacks of the nodes inside it), and a module's after all others.
Beyond that, the order in which callbacks run is unspecified: for example, Ruff visits the bodies of
functions after the rest of the module. A callback that cares about the order of nodes in the file can
collect them, and sort them by `line` in an `on_exit` callback.

Callbacks see the nodes that Ruff's own rules see. In particular, Ruff parses quoted annotations, like
`"List[int]"`, so callbacks see the expressions inside them. They also see the string as a `Constant`,
except where Ruff treats every annotation as a string: in stubs, with `from __future__ import annotations`,
and for Python 3.14 and later.

### Node kinds

Kinds are named after the classes in Python's `ast` module, and have the same fields. Fields marked with
`()` hold nodes, and are methods; the others are attributes. Fields that hold operators (like `BinOp.op`)
or contexts (like `Name.ctx`) hold the name of the operator or context class, like `"Add"` or `"Load"`.

| Statements                        | Fields                                                                |
| --------------------------------- | --------------------------------------------------------------------- |
| `FunctionDef`, `AsyncFunctionDef` | `name`, `args()`, `body()`, `decorator_list()`, `returns()`           |
| `ClassDef`                        | `name`, `bases()`, `keywords()`, `body()`, `decorator_list()`         |
| `Return`                          | `value()`                                                             |
| `Delete`                          | `targets()`                                                           |
| `Assign`                          | `targets()`, `value()`                                                |
| `TypeAlias`                       | `name()`, `value()`                                                   |
| `AugAssign`                       | `target()`, `op`, `value()`                                           |
| `AnnAssign`                       | `target()`, `annotation()`, `value()`, `simple`                       |
| `For`, `AsyncFor`                 | `target()`, `iter()`, `body()`, `orelse()`                            |
| `While`                           | `test()`, `body()`, `orelse()`                                        |
| `If`                              | `test()`, `body()`, `orelse()` (an `elif` is an `If` in the `orelse`) |
| `With`, `AsyncWith`               | `items()`, `body()`                                                   |
| `Match`                           | `subject()`, `cases()`                                                |
| `Raise`                           | `exc()`, `cause()`                                                    |
| `Try`, `TryStar`                  | `body()`, `handlers()`, `orelse()`, `finalbody()`                     |
| `Assert`                          | `test()`, `msg()`                                                     |
| `Import`                          | `names()`                                                             |
| `ImportFrom`                      | `module`, `names()`, `level`                                          |
| `Global`, `Nonlocal`              | `names`                                                               |
| `Expr`                            | `value()`                                                             |
| `Pass`, `Break`, `Continue`       |                                                                       |

| Expressions                           | Fields                                               |
| ------------------------------------- | ---------------------------------------------------- |
| `BoolOp`                              | `op`, `values()`                                     |
| `NamedExpr`                           | `target()`, `value()`                                |
| `BinOp`                               | `left()`, `op`, `right()`                            |
| `UnaryOp`                             | `op`, `operand()`                                    |
| `Lambda`                              | `args()`, `body()`                                   |
| `IfExp`                               | `test()`, `body()`, `orelse()`                       |
| `Dict`                                | `keys()`, `values()`                                 |
| `Set`                                 | `elts()`                                             |
| `ListComp`, `SetComp`, `GeneratorExp` | `elt()`, `generators()`                              |
| `DictComp`                            | `key()`, `value()`, `generators()`                   |
| `Await`, `Yield`, `YieldFrom`         | `value()`                                            |
| `Compare`                             | `left()`, `ops`, `comparators()`                     |
| `Call`                                | `func()`, `args()`, `keywords()`                     |
| `JoinedStr`, `TemplateStr`            | (f-strings and t-strings; no fields yet, use `text`) |
| `Constant`                            | `value`                                              |
| `Attribute`                           | `value()`, `attr`, `ctx`                             |
| `Subscript`                           | `value()`, `slice()`, `ctx`                          |
| `Starred`                             | `value()`, `ctx`                                     |
| `Name`                                | `id`, `ctx`                                          |
| `List`, `Tuple`                       | `elts()`, `ctx`                                      |
| `Slice`                               | `lower()`, `upper()`, `step()`                       |

| Other nodes     | Fields                                                                                          |
| --------------- | ----------------------------------------------------------------------------------------------- |
| `ExceptHandler` | `type()`, `name`, `body()`                                                                      |
| `arguments`     | `posonlyargs()`, `args()`, `vararg()`, `kwonlyargs()`, `kw_defaults()`, `kwarg()`, `defaults()` |
| `arg`           | `arg`, `annotation()`, `default()` (unlike Python's `ast`, an `arg` knows its default)          |
| `keyword`       | `arg`, `value()`                                                                                |
| `alias`         | `name`, `asname`                                                                                |
| `withitem`      | `context_expr()`, `optional_vars()`                                                             |
| `comprehension` | `target()`, `iter()`, `ifs()`, `is_async`                                                       |
| `match_case`    | `guard()`, `body()`                                                                             |
| `Comment`       | `text` (from `comments(...)`)                                                                   |

Fields that hold match patterns and type parameters, and `type_comment` fields, aren't available yet.

## Performance

Selectors are evaluated natively during the single pass that Ruff already makes over each file, so a
callback only runs for the nodes it selects. Path filters are checked once per file, and checks that
don't apply to a file cost nothing in it. Each callback call costs a little more than a native filter, so
select as narrowly as possible: `nodes("Attribute").filter_name("^info$")` calls its callback far less
often than `nodes("Call")` with the same check in Starlark.

Nodes are handles to Ruff's syntax tree: a node's attributes are copied when it's passed to a callback, and its methods only read what they return.

## Examples

Ruff's test suite includes a set of plugins for common project-specific rules, in
[`crates/ruff_linter/resources/test/fixtures/plugins`](../crates/ruff_linter/resources/test/fixtures/plugins):

| Plugin                    | Rules                                                                                          |
| ------------------------- | ---------------------------------------------------------------------------------------------- |
| `banned_api.star`         | Bans APIs, with a reason and an alternative for each (configurable).                           |
| `layers.star`             | Enforces a layered architecture, like [import-linter](https://import-linter.readthedocs.io/).  |
| `feature_flags.star`      | Replaces checks of launched or removed feature flags with constants.                           |
| `api_migration.star`      | Migrates calls of a deprecated internal API, converting an argument's units.                   |
| `test_hygiene.star`       | Flags sleeping tests, tests without assertions, and skips without reasons, in test files only. |
| `routes.star`             | Flags URL routes that a `urls.py` defines twice.                                               |
| `todo_tickets.star`       | Requires a ticket in every TODO comment.                                                       |
| `async_blocking.star`     | Flags a project's blocking functions when called from coroutines.                              |
| `django_models.star`      | Requires Django models to define `__str__`.                                                    |
| `structured_logging.star` | Requires structured log events to be snake_case literals.                                      |
| `env_access.star`         | Requires reading the environment through a settings module.                                    |
| `slop.star`               | Flags comments that narrate code and placeholder functions.                                    |

## Limitations

- Plugin rules are enabled by listing their plugin. They can't yet be selected or ignored with `select`,
    `ignore`, or `per-file-ignores` (Ruff warns if they are); use path filters, or `# noqa` comments.
    `ruff rule` doesn't know about them either.
- An inherited plugin can't be removed, only replaced.
- Plugins can't `load()` other Starlark files.
- Plugins see one file at a time.
- Plugins can't yet query Ruff's semantic model beyond qualified names, such as a name's bindings or
    whether it's used.
- Plugins are code that Ruff runs, and Ruff trusts them like the rest of its configuration: there's no
    limit on how long a callback runs or how much memory it uses, so a plugin that loops forever hangs Ruff.
    The values that a plugin's callbacks create for a file are kept until Ruff has finished the file.
- The language server reloads plugins when the configuration changes, but not when only a plugin's file
    does.
- `Constant` nodes don't have the `kind` field of Python's `ast` (`"u"` for a `u"..."` string).

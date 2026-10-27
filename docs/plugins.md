# Plugins

> **Preview:** Plugins are a [preview](https://docs.astral.sh/ruff/preview/) feature, and their API
> may change.

Plugins add project-specific lint rules to Ruff. They're written in
[Starlark](https://github.com/bazelbuild/starlark), a small, deterministic dialect of Python. A plugin is
a `.star` file in your repository: there's nothing to build or install, and no Python interpreter runs
at lint time.

```python
# lint/http.star
TIMEOUT = rule("HTTP001", "http-request-without-timeout")

def check(call):
    if not call.keyword("timeout"):
        TIMEOUT.report(call, "`{}` has no `timeout`, so it can hang forever".format(call.func().text))

calls("requests.get", "requests.post").foreach(check)
```

```toml
# ruff.toml
[lint]
preview = true

[lint.plugins]
http = "lint/http.star"
```

```python
import requests
from requests import get as fetch

requests.get(url)              # HTTP001
fetch(url)                     # HTTP001: `fetch` refers to `requests.get`
requests.get(url, timeout=10)  # OK
```

## How it works

A plugin does three things:

- **It declares rules** with `rule(code, name)`. Their diagnostics work like those of Ruff's own rules:
  they're suppressed with `# noqa: HTTP001`, cached, shown in editors, and fixed by `ruff check --fix`.
- **It selects nodes** with selectors like `calls("requests.get")`, `nodes("FunctionDef")`, `imports()`,
  `comments("TODO")`, and `modules()`, narrowed with filters like `.filter_path("tests/**")` and
  `.filter_name("^test_")`. Ruff evaluates selectors natively, during the pass it already makes over
  each file, and resolves names through imports and aliases the way its own rules do.
- **It registers callbacks** with `.foreach(callback)`. A callback is plain Starlark that runs on each
  selected node, and reports diagnostics, with optional fixes.

Nodes look like those of Python's [`ast`](https://docs.python.org/3/library/ast.html) module, with one
rule: fields that hold plain data are attributes, and fields that hold other nodes are methods. So
`function.name` is a string and `function.body()` is a list of statements, and `call.func().attr` is the
method name in `client.get(url)`. `type(node)` is the node's kind, like `"Call"`.

## Examples

### State, and checks scoped to a node

Like Ruff's own rules, a plugin can gather information while Ruff visits part of a file, and decide what
to report once Ruff is done with it. A callback can select the nodes inside the node it received, and
register an `on_exit` callback that runs after them:

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

nodes("FunctionDef").filter_path("tests/**").filter_name("^test_").foreach(check_test)
```

### Fixes and options

`report` takes a fix, made of edits like `replace(node, text)` and `delete(node)`. Edits know Python's
syntax, as Ruff's own fixes do: deleting an argument deletes its comma, and deleting the only statement
of a block leaves `pass`. Plugins can also declare options, which are set in the configuration:

```python
LAUNCHED = option("launched", default = [], help = "Feature flags that are now always on.")

LAUNCHED_FLAG = rule("FLAG001", "launched-feature-flag")

def check(call):
    flag = call.argument("name", 0)
    if flag and flag.literal() in LAUNCHED:
        LAUNCHED_FLAG.report(
            call,
            "The feature flag `{}` has launched, so this check is always `True`".format(flag.literal()),
            fix = replace(call, "True"),
            fix_title = "Replace with `True`",
        )

calls("acme.flags.is_enabled").foreach(check)
```

```toml
[lint.plugins]
flags = { path = "lint/flags.star", options = { launched = ["new-checkout"] } }
```

### Per-file state

A `modules()` callback runs once per file, before Ruff visits the file's nodes, so it can compute what the
file's checks have in common, and register those checks. This plugin keeps the layers of an application
from importing the layers above them:

```python
LAYERS = option("layers", default = [], help = "Module prefixes, from the highest layer to the lowest.")

LAYER_VIOLATION = rule("ARCH001", "layer-violation")

def layer_of(module):
    for index, layer in enumerate(LAYERS):
        if module == layer or module.startswith(layer + "."):
            return index
    return None

def check_module(module):
    here = layer_of(module.name) if module.name else None
    if here == None:
        return

    def check_import(alias):
        there = layer_of(alias.qualname)
        if there != None and there < here:
            LAYER_VIOLATION.report(alias, "`{}` can't import `{}`, from a higher layer".format(
                module.name,
                alias.qualname,
            ))

    module.imports().foreach(check_import)

modules().foreach(check_module)
```

### Comments

```python
TODO_WITHOUT_TICKET = rule("TODO001", "todo-without-ticket")

def check(comment):
    if not re.search(r"TODO\([A-Z]+-[0-9]+\)", comment.text):
        TODO_WITHOUT_TICKET.report(comment, "Link this TODO to a ticket, like `TODO(ABC-123): ...`")

comments(r"\bTODO\b").foreach(check)
```

Ruff's test suite has [more example plugins](../crates/ruff_linter/resources/test/fixtures/plugins), for
banned APIs, API migrations, Django models, structured logging, blocking calls in coroutines, and more.

## Performance

Callbacks only run for the nodes their selectors select, and checks that a path filter excludes from a
file cost nothing there. On the CPython standard library, most of the example plugins add 2–10% to the
work Ruff does.

## Learn more

The [plugin API reference](plugin-api.md) describes everything plugins can do: every selector, node kind,
and edit, the order of callbacks, Starlark for Python programmers, errors, and current limitations.

# Conventions for a pytest suite. The checks only apply to test files: path filters are checked
# once per file, so they cost nothing elsewhere.

TESTS = ["tests/**", "**/test_*.py"]

SLEEP = rule("TST001", "sleep-in-test")
NO_ASSERTION = rule("TST002", "test-without-assertion")
SKIP_WITHOUT_REASON = rule("TST003", "skip-without-reason")

calls("time.sleep", "asyncio.sleep").filter_path(*TESTS).foreach(
    lambda call: SLEEP.report(call, "Tests shouldn't sleep; wait for a condition or use a fake clock"),
)

# A test passes if it doesn't raise, so a test without any assertion often checks nothing.
def check_test(function):
    assertions = []

    # These checks are scoped to the test: they only see the nodes inside it.
    function.nodes("Assert").foreach(assertions.append)
    function.references().filter_name(r"^(assert\w*|raises|warns|fail)$").foreach(assertions.append)

    # `on_exit` runs once ruff has visited the whole test, and the checks above have seen it.
    function.on_exit(lambda: assertions or NO_ASSERTION.report(
        function.span("name"),
        "`{}` doesn't assert anything".format(function.name),
    ))

nodes("FunctionDef", "AsyncFunctionDef").filter_path(*TESTS).filter_name("^test_").foreach(check_test)

def check_skip(call):
    if call.argument("reason", 0) == None:
        SKIP_WITHOUT_REASON.report(call, "Explain why the test is skipped with `reason=`")

calls("pytest.mark.skip", "pytest.mark.xfail", "pytest.skip").filter_path(*TESTS).foreach(check_skip)

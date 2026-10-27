# Clean up feature flags that have been fully rolled out (or rolled back): checks of a removed
# flag become constants, which the rest of ruff (and a human) can then simplify.
#
#     [tool.ruff.lint.plugins.feature-flags]
#     path = "lint/feature_flags.star"
#     options = { launched = ["new-checkout"], removed = ["legacy-search"] }

LAUNCHED = option("launched", default = [], help = "Flags that are now always on.")
REMOVED = option("removed", default = [], help = "Flags that are now always off.")

LAUNCHED_FLAG = rule("FLAG001", "launched-feature-flag")
REMOVED_FLAG = rule("FLAG002", "removed-feature-flag")

def check(call):
    flag = call.argument("name", 0)
    flag = flag.literal() if flag else None
    if flag in LAUNCHED:
        LAUNCHED_FLAG.report(
            call,
            "The feature flag `{}` has launched, so this check is always `True`".format(flag),
            fix = replace(call, "True"),
            fix_title = "Replace with `True`",
        )
    elif flag in REMOVED:
        REMOVED_FLAG.report(
            call,
            "The feature flag `{}` was removed, so this check is always `False`".format(flag),
            fix = replace(call, "False"),
            fix_title = "Replace with `False`",
        )

calls("acme.flags.is_enabled").foreach(check)

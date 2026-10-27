# Each URL route is defined once per `urls.py`: a second definition is dead, because Django uses
# the first one that matches.

DUPLICATE_ROUTE = rule("WEB001", "duplicate-route")

# State lives in the variables of a `modules()` callback, so each file gets its own.
def check_module(module):
    seen = {}

    def check(call):
        route = call.argument("route", 0)
        if route == None or type(route.literal()) != "string":
            return
        if route.literal() in seen:
            DUPLICATE_ROUTE.report(route, "`{}` is already routed on line {}".format(
                route.literal(),
                seen[route.literal()].line,
            ))
        else:
            seen[route.literal()] = call

    module.calls("django.urls.path", "django.urls.re_path").foreach(check)

modules().filter_path("**/urls.py").foreach(check_module)

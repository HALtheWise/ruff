# Configuration is read from the environment in exactly one place, so that every setting is
# documented, typed, and validated at startup.

ENVIRONMENT_ACCESS = rule("ENV001", "environment-access")

references("os.environ", "os.getenv", "os.environb", "os.putenv").exclude_path("**/settings.py").foreach(
    lambda name: ENVIRONMENT_ACCESS.report(
        name,
        "Read configuration from `app.settings` instead of `{}`".format(name.qualname),
    ),
)

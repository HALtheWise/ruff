from acme import trace


def classify(a, b, c: int, *args, key=None, **kwargs):
    if a:
        return 1
    elif b:
        return 2
    else:
        return 3


def run(task):
    task(1, debug=True)
    task(debug=True)
    task(debug=False, retries=5)
    task(retries=-1)
    task(retries=2)  # OK


class Handlers:
    def handle_login(self, request):
        breakpoint()

    @trace
    def handle_logout(self, request):  # OK: traced
        breakpoint()
        return None

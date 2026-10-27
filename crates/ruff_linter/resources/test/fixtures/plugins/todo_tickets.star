# Every TODO must link to a ticket, so that it can be found and scheduled.

TICKET = option(
    "ticket",
    default = r"[A-Z][A-Z0-9]+-[0-9]+",
    help = "A regular expression for ticket identifiers, like `ABC-123`.",
)

TODO_WITHOUT_TICKET = rule("TODO001", "todo-without-ticket")

def check(comment):
    if not re.search(r"\b(TODO|FIXME|XXX)\(" + TICKET + r"\)", comment.text):
        TODO_WITHOUT_TICKET.report(comment, "Link this TODO to a ticket, like `TODO(ABC-123): ...`")

comments(r"\b(TODO|FIXME|XXX)\b").foreach(check)

# Every Django model defines `__str__`, so that it renders usefully in the admin and in logs.

MODEL_WITHOUT_STR = rule("DJM001", "model-without-str")

def is_abstract_meta(stmt):
    """Whether `stmt` is `class Meta: abstract = True`."""
    if type(stmt) != "ClassDef" or stmt.name != "Meta":
        return False
    for item in stmt.body():
        if type(item) == "Assign" and [target.text for target in item.targets()] == ["abstract"]:
            return item.value().literal() == True
    return False

def defines_str(stmt):
    return type(stmt) == "FunctionDef" and stmt.name == "__str__"

def check(cls):
    if not any([base.qualname == "django.db.models.Model" for base in cls.bases()]):
        return
    # Abstract models leave `__str__` to their subclasses.
    if any([is_abstract_meta(stmt) or defines_str(stmt) for stmt in cls.body()]):
        return
    MODEL_WITHOUT_STR.report(cls.span("name"), "The model `{}` doesn't define `__str__`".format(cls.name))

nodes("ClassDef").foreach(check)

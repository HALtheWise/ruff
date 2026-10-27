# Layered architecture, in the style of import-linter: each module may import from its own
# layer and the layers below it, but never from a layer above it.
#
#     [tool.ruff.lint.plugins.layers]
#     path = "lint/layers.star"
#     options = { layers = ["app.api", "app.services", "app.domain"] }

LAYERS = option(
    "layers",
    default = [],
    help = "Module prefixes, from the highest layer to the lowest.",
)

LAYER_VIOLATION = rule("ARCH001", "layer-violation")

def layer_of(module):
    """The index of the layer containing `module`, or `None`."""
    for index, layer in enumerate(LAYERS):
        if module == layer or module.startswith(layer + "."):
            return index
    return None

# `modules()` callbacks run before ruff visits the file's nodes, so they can compute what the
# file's checks have in common, and register those checks.
def check_module(module):
    here = layer_of(module.name) if module.name else None
    if here == None:
        # Scripts, and modules outside of any layer, can import anything.
        return

    def check_import(alias):
        there = layer_of(alias.qualname)
        if there != None and there < here:
            LAYER_VIOLATION.report(
                alias.parent(),
                "`{}` is in layer `{}`, so it can't import `{}` from the higher layer `{}`".format(
                    module.name,
                    LAYERS[here],
                    alias.qualname,
                    LAYERS[there],
                ),
            )

    module.imports().foreach(check_import)

modules().foreach(check_module)

from acme import flags
from acme.flags import is_enabled

if flags.is_enabled("new-checkout"):
    pass

if is_enabled(name="legacy-search", user=None):
    pass

if flags.is_enabled("dark-mode"):  # OK: still rolling out
    pass

flag = "new-checkout"
if flags.is_enabled(flag):  # OK: not a literal
    pass

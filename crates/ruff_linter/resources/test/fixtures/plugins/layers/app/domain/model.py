import json  # OK: not in any layer
from app.domain import values  # OK: same layer
from app.services.billing import charge  # ARCH001
import app.api.views  # ARCH001

from ..services import billing  # ARCH001: relative imports are resolved

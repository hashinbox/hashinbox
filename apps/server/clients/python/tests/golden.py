"""The shared vectors (`apps/server/clients/_golden/`)."""

import json
from pathlib import Path
from typing import Any

GOLDEN = Path(__file__).resolve().parents[2] / "_golden"


def load(name: str) -> Any:
    return json.loads((GOLDEN / name).read_text())

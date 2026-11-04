#!/usr/bin/env python3
"""Checks that no module of apps/server imports upward (docs/design/069-server-layers.md).

Layers, bottom up: models, usecase, api, main (`main.rs`, `config.rs`, `ui.rs`).
`db` sits beside them and depends on no other module. A module may name,
through `crate::`, only its own layer, the layers below it, and `db`; `main`
names everything. Comments are ignored.

usage: scripts/server-layers.py        (exits 1 and lists every upward import)
"""

import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SOURCE = os.path.join(ROOT, "apps", "server", "src")

BESIDE = {"db"}
# The `crate::` roots each top-level module may name.
ALLOWED = {
    "db": set(),
    "models": {"models"} | BESIDE,
    "usecase": {"usecase", "models"} | BESIDE,
    "api": {"api", "usecase", "models"} | BESIDE,
}
MAIN = {"main", "config", "ui"}


def roots(text):
    """Every top-level module a `crate::` path in `text` starts with."""
    found = set()
    for match in re.finditer(r"\bcrate::", text):
        rest = text[match.end():].lstrip()
        if rest.startswith("{"):
            depth, group = 0, ""
            for char in rest:
                depth += {"{": 1, "}": -1}.get(char, 0)
                group += char
                if depth == 0:
                    break
            # Only the group's first level: names after `{` or a top-level `,`.
            level, start = 0, True
            for token in re.findall(r"[{},]|\w+", group):
                if token == "{":
                    level += 1
                    start = level == 1
                elif token == "}":
                    level -= 1
                elif token == ",":
                    start = level == 1
                elif start:
                    found.add(token)
                    start = False
        else:
            name = re.match(r"\w+", rest)
            if name:
                found.add(name.group(0))
    return found


def main():
    upward = []
    for directory, _, files in os.walk(SOURCE):
        for file in sorted(files):
            if not file.endswith(".rs"):
                continue
            path = os.path.join(directory, file)
            module = os.path.relpath(path, SOURCE).split(os.sep)[0].removesuffix(".rs")
            if module in MAIN:
                continue
            if module not in ALLOWED:
                upward.append(f"{os.path.relpath(path, ROOT)}: {module} is in no layer")
                continue
            with open(path, encoding="utf-8") as source:
                text = re.sub(r"//.*", "", source.read())
            for root in sorted(roots(text) - ALLOWED[module] - {"self"}):
                upward.append(f"{os.path.relpath(path, ROOT)}: {module} names crate::{root}")
    for line in upward:
        print(line)
    return 1 if upward else 0


if __name__ == "__main__":
    sys.exit(main())

#!/usr/bin/env python3
"""Draws the workspace's crate dependency map as one self-contained HTML page.

Nothing here is written by hand: crates, folders, features and dependencies come
from `cargo metadata`, a crate's domain is the first two segments of its path
(`sdk/chains/bitcoin` belongs to `sdk/chains`), and rows, order and edge routes
are computed from the graph.

usage: scripts/crate-map.py [output.html]        (default: target/crate-map.html)
"""

import json
import os
import subprocess
import sys
from collections import defaultdict

NW, NH = 150, 46  # one crate box
PAD = 14  # between a domain box and what it holds
GAP = 22  # between two boxes of one row
LANE = 14  # what a passing bundle of edges takes in a row
LANE_GAP = 8
PITCH = 140  # row to row
MARGIN = 24
SWEEPS = 24

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TEMPLATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "crate-map.template.html")


def run(*command):
    return subprocess.run(command, cwd=ROOT, check=True, capture_output=True, text=True).stdout


def load():
    """Every workspace crate: its folder, features, and workspace dependencies."""
    meta = json.loads(run("cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"))
    names = {package["name"] for package in meta["packages"]}
    crates = {}
    for package in meta["packages"]:
        folder = os.path.relpath(os.path.dirname(package["manifest_path"]), meta["workspace_root"])
        folder = folder.replace(os.sep, "/")
        deps, dev = set(), set()
        for dep in package["dependencies"]:
            if dep["name"] not in names:
                continue
            if dep["kind"] == "dev":
                features = ", ".join(dep.get("features", []))
                dev.add(dep["name"] + (f" [{features}]" if features else ""))
            elif dep["name"] != package["name"]:
                deps.add(dep["name"])
        crates[package["name"]] = {
            "path": folder,
            "domain": "/".join(folder.split("/")[:2]),
            "features": sorted(f for f in package["features"] if f != "default"),
            "deps": sorted(deps),
            "dev": sorted(dev),
        }
    return crates


def closure(crates):
    """Every workspace crate that ends up in each crate's build."""
    memo = {}

    def reach(name):
        if name not in memo:
            memo[name] = set()
            for dep in crates[name]["deps"]:
                memo[name] |= {dep} | reach(dep)
        return memo[name]

    return {name: reach(name) for name in crates}


def levels(crates, domains):
    """A domain's row: one above the highest domain it depends on."""
    needs = defaultdict(set)
    for crate in crates.values():
        for dep in crate["deps"]:
            if crates[dep]["domain"] != crate["domain"]:
                needs[crate["domain"]].add(crates[dep]["domain"])
    level, walking = {}, set()

    def of(domain):
        if domain in level:
            return level[domain]
        if domain in walking:
            sys.exit(f"crate-map: folders depend on each other in a cycle through {domain}")
        walking.add(domain)
        level[domain] = 1 + max((of(d) for d in needs[domain]), default=-1)
        walking.discard(domain)
        return level[domain]

    for domain in domains:
        of(domain)
    return level, needs


class Layout:
    """Rows of domain boxes, with a lane wherever an edge passes through a row."""

    def __init__(self, crates, domains, level):
        self.crates = crates
        self.top = max(level.values())
        self.rows = {row: [] for row in range(self.top + 1)}
        self.box, self.offset, self.lanes = {}, {}, {}
        for name in sorted(domains):
            members = domains[name]
            label = round(len(name) * 7.2) + PAD
            width = PAD + label + len(members) * NW + (len(members) - 1) * GAP + PAD
            item = {"kind": "domain", "id": name, "w": width, "x": 0.0, "label": label,
                    "points": [("c", member) for member in members]}
            self.box[name] = item
            self.rows[level[name]].append(item)
            for index, member in enumerate(members):
                self.offset[member] = -width / 2 + PAD + label + NW / 2 + index * (NW + GAP)
                crates[member]["row"] = level[name]
        self.routes, self.near = {}, defaultdict(set)
        for source in sorted(crates):
            for target in crates[source]["deps"]:
                above, below = crates[source]["row"], crates[target]["row"]
                route = [("c", source)]
                route += [("l", row, target) for row in range(above - 1, below, -1)]
                route.append(("c", target))
                self.routes[(source, target)] = route
                for point in route[1:-1]:
                    if point not in self.lanes:
                        self.lanes[point] = {"kind": "lane", "id": point, "w": LANE, "x": 0.0,
                                             "points": [point]}
                        self.rows[point[1]].append(self.lanes[point])
                for one, other in zip(route, route[1:]):
                    self.near[one].add(other)
                    self.near[other].add(one)

    def x(self, point):
        if point[0] == "c":
            return self.box[self.crates[point[1]]["domain"]]["x"] + self.offset[point[1]]
        return self.lanes[point]["x"]

    def row(self, point):
        return self.crates[point[1]]["row"] if point[0] == "c" else point[1]

    def y(self, row):
        return MARGIN + PAD + NH / 2 + (self.top - row) * PITCH

    def want(self, item, rows):
        """Where an item would sit if its edges into `rows` ran straight."""
        pulls = []
        for point in item["points"]:
            shift = self.offset[point[1]] if point[0] == "c" else 0.0
            pulls += [self.x(n) - shift for n in self.near[point] if self.row(n) in rows]
        return sum(pulls) / len(pulls) if pulls else item["x"]

    @staticmethod
    def place(row):
        """The closest each item gets to where it wants to be without overlapping."""
        def apart(one, other):
            gap = GAP if "domain" in (one["kind"], other["kind"]) else LANE_GAP
            return (one["w"] + other["w"]) / 2 + gap

        count = len(row)
        left, right = [0.0] * count, [0.0] * count
        for i in range(count):
            left[i] = row[i]["want"]
            if i:
                left[i] = max(left[i], left[i - 1] + apart(row[i - 1], row[i]))
        for i in range(count - 1, -1, -1):
            right[i] = row[i]["want"]
            if i < count - 1:
                right[i] = min(right[i], right[i + 1] - apart(row[i], row[i + 1]))
        for i in range(count):
            row[i]["x"] = (left[i] + right[i]) / 2

    def arrange(self):
        for row in self.rows.values():
            for item in row:
                item["want"] = 0.0
            self.place(row)
        order = sorted(self.rows, reverse=True)
        for sweep in range(SWEEPS):
            down = sweep % 2 == 0
            for row in (order if down else order[::-1])[1:]:
                for item in self.rows[row]:
                    item["want"] = self.want(item, {row + 1 if down else row - 1})
                self.rows[row].sort(key=lambda item: item["want"])
                self.place(self.rows[row])
        for _ in range(6):
            for row in order:
                for item in self.rows[row]:
                    item["want"] = self.want(item, {row - 1, row + 1})
                self.place(self.rows[row])
        items = [item for row in self.rows.values() for item in row]
        shift = MARGIN - min(item["x"] - item["w"] / 2 for item in items)
        for item in items:
            item["x"] += shift
        for row in self.rows.values():
            for one, other in zip(row, row[1:]):
                assert one["x"] + one["w"] / 2 <= other["x"] - other["w"] / 2, "two boxes overlap"
        self.width = max(item["x"] + item["w"] / 2 for item in items) + MARGIN
        self.height = 2 * MARGIN + self.top * PITCH + NH + 2 * PAD

    def anchors(self):
        """Where each edge leaves and enters a crate, spread along its edge."""
        leaving, entering = defaultdict(set), defaultdict(set)
        for (source, target), route in self.routes.items():
            if self.crates[source]["row"] != self.crates[target]["row"]:
                leaving[source].add(route[1])
                entering[target].add(route[-2])
        spots = {}
        for side, groups in (("out", leaving), ("in", entering)):
            for crate, points in groups.items():
                ordered = sorted(points, key=lambda point: (self.x(point), str(point)))
                step = min(14.0, (NW - 36) / max(1, len(ordered) - 1))
                for index, point in enumerate(ordered):
                    spots[(side, crate, point)] = (
                        self.x(("c", crate)) + (index - (len(ordered) - 1) / 2) * step)
        return spots

    def paths(self):
        spots = self.anchors()

        def curve(a, b):
            bend = (b[1] - a[1]) * 0.5
            return f"C{a[0]:.1f},{a[1] + bend:.1f} {b[0]:.1f},{b[1] - bend:.1f} {b[0]:.1f},{b[1]:.1f}"

        drawn = {}
        for (source, target), route in self.routes.items():
            above, below = self.crates[source]["row"], self.crates[target]["row"]
            if above == below:
                a = (self.x(route[0]), self.y(above) + NH / 2)
                b = (self.x(route[-1]), self.y(below) + NH / 2 + 2)
                drawn[(source, target)] = (
                    f"M{a[0]:.1f},{a[1]:.1f} C{a[0]:.1f},{a[1] + 44:.1f} "
                    f"{b[0]:.1f},{b[1] + 44:.1f} {b[0]:.1f},{b[1]:.1f}")
                continue
            here = (spots[("out", source, route[1])], self.y(above) + NH / 2)
            path = f"M{here[0]:.1f},{here[1]:.1f}"
            for point in route[1:-1]:
                middle = self.y(point[1])
                enter = (self.x(point), middle - NH / 2 - PAD)
                leave = (self.x(point), middle + NH / 2 + PAD)
                path += f" {curve(here, enter)} L{leave[0]:.1f},{leave[1]:.1f}"
                here = leave
            end = (spots[("in", target, route[-2])], self.y(below) - NH / 2 - 2)
            drawn[(source, target)] = f"{path} {curve(here, end)}"
        return drawn


def facts(crates, domains, level, reach, users, implied):
    """What the numbers say, stated from the graph."""
    code = lambda name: f"<code>{name}</code>"
    rows = defaultdict(list)
    for domain, row in level.items():
        rows[row].append(domain)
    said = ["Layers, from the bottom up: "
            + " → ".join(", ".join(code(d) for d in sorted(rows[r])) for r in sorted(rows))
            + ". Every arrow points down a layer, so no folder depends on one above it."]
    declared = sum(len(c["deps"]) for c in crates.values())
    said.append(f"{len(implied)} of the {declared} declared dependencies name a crate that is "
                "already in the build through another dependency.")
    busiest = sorted(crates, key=lambda n: (-len(users[n]), n))[:3]
    said.append("Most depended on: " + ", ".join(
        f"{code(n)} ({len(users[n])} of {len(crates) - 1} crates)" for n in busiest) + ".")
    heaviest = sorted(crates, key=lambda n: (-len(reach[n]), n))[0]
    said.append(f"Largest build: {code(heaviest)} links {len(reach[heaviest])} workspace crates.")
    for domain in sorted(domains):
        members = domains[domain]
        if len(members) < 2:
            continue
        tied = [(m, d) for m in members for d in crates[m]["deps"] if d in members]
        said.append(
            f"No crate in {code(domain)} depends on another crate in {code(domain)}." if not tied
            else f"Inside {code(domain)}: " + ", ".join(f"{code(m)} uses {code(d)}" for m, d in tied) + ".")
    for domain in sorted(d for d in domains if d.startswith("sdk/")):
        pulled = sorted({crates[r]["domain"] for m in domains[domain] for r in reach[m]} - {domain})
        if pulled:
            said.append(f"Linking {code(domain)} builds " + ", ".join(code(p) for p in pulled) + ".")
    return said


def main():
    output = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "crate-map.html")
    crates = load()
    domains = defaultdict(list)
    for name in sorted(crates):
        domains[crates[name]["domain"]].append(name)
    level, needs = levels(crates, domains)
    reach = closure(crates)
    users = defaultdict(list)
    for name in sorted(crates):
        for dep in crates[name]["deps"]:
            users[dep].append(name)
    implied = {(s, t) for s in crates for t in crates[s]["deps"]
               if any(w != t and t in reach[w] for w in crates[s]["deps"])}

    layout = Layout(crates, domains, level)
    layout.arrange()
    drawn = layout.paths()

    commit = run("git", "rev-parse", "--short", "HEAD").strip()
    if run("git", "status", "--porcelain").strip():
        commit += " with uncommitted changes"
    used_by = defaultdict(set)
    for domain, others in needs.items():
        for other in others:
            used_by[other].add(domain)
    top_down = sorted(domains, key=lambda d: (-level[d], layout.box[d]["x"]))
    data = {
        "w": round(layout.width), "h": round(layout.height), "nw": NW, "nh": NH, "commit": commit,
        "groups": [{
            "label": d, "sdk": d.startswith("sdk/"),
            "x": round(layout.box[d]["x"] - layout.box[d]["w"] / 2, 1),
            "y": round(layout.y(level[d]) - NH / 2 - PAD, 1),
            "w": layout.box[d]["w"], "h": NH + 2 * PAD,
        } for d in top_down],
        "nodes": [{
            "id": n, "path": c["path"], "sdk": c["domain"].startswith("sdk/"),
            "x": round(layout.x(("c", n)), 1), "y": round(layout.y(c["row"]), 1),
            "features": c["features"], "dev": c["dev"], "uses": c["deps"],
            "usedBy": users[n], "build": sorted(reach[n]),
        } for n, c in sorted(crates.items())],
        "edges": [{"from": s, "to": t, "implied": (s, t) in implied, "d": d}
                  for (s, t), d in sorted(drawn.items())],
        "domains": [{
            "label": d, "sdk": d.startswith("sdk/"), "crates": domains[d],
            "needs": [o for o in top_down if o in needs[d]],
            "usedBy": [o for o in top_down if o in used_by[d]],
        } for d in top_down],
        "facts": facts(crates, domains, level, reach, users, implied),
    }
    with open(TEMPLATE, encoding="utf-8") as template:
        page = template.read().replace("__DATA__", json.dumps(data).replace("</", "<\\/"))
    os.makedirs(os.path.dirname(os.path.abspath(output)), exist_ok=True)
    with open(output, "w", encoding="utf-8") as out:
        out.write(page)
    print(f"{output}: {len(crates)} crates in {len(domains)} folders, {len(drawn)} dependencies, "
          f"{len(implied)} already in the build, {layout.width:.0f}x{layout.height:.0f}")


if __name__ == "__main__":
    main()

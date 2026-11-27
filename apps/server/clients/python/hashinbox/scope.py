"""Hand-written: packages/scope ported. A scope is text
(`com.hashinbox.wallets[w1]:read`): a path of resources, full or read only. One
covers another, a set packs into the JWT `scopes` claim, and a template's
`{name}` holes are filled with escaped ids. The vectors in
`apps/server/clients/_golden/scope.json` keep this port honest."""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from dataclasses import dataclass
from typing import TypeAlias

__all__ = [
    "Packed",
    "Scope",
    "ScopeError",
    "Scopes",
    "Segment",
    "escape_id",
    "fill",
]

# What a packed set is: each shared prefix and the scopes after it.
Packed: TypeAlias = Mapping[str, Iterable[str]]

# What a read-only scope's text ends with.
_READ = ":read"

_NAME = re.compile(r"[A-Za-z0-9_-]+")
_HEX = re.compile(rb"[0-9A-Fa-f]{2}|\+[0-9A-Fa-f]")
# Rust's `char::is_whitespace`, what `split_whitespace` splits on.
_WHITESPACE = re.compile("[\t\n\x0b\x0c\r \x85\xa0  -     　]+")


class ScopeError(ValueError):
    """A scope's text that does not parse."""


def _is_name(text: str) -> bool:
    return _NAME.fullmatch(text) is not None


def escape_id(id: str) -> str:
    """`id` as written between a scope's brackets: `%`, `[`, `]`, whitespace
    and controls as `%XX`."""
    return re.sub(r"[%\[\]\x00-\x20\x7f]", lambda m: f"%{ord(m[0]):02X}", id)


def _unescape(text: str) -> str:
    """The id `text` (as written between `[` and `]`) is."""
    raw = text.encode()
    out = bytearray()
    at = 0
    while at < len(raw):
        if raw[at] != ord("%"):
            out.append(raw[at])
            at += 1
            continue
        hex = raw[at + 1 : at + 3]
        if _HEX.fullmatch(hex) is None:
            raise ScopeError(f"bad escape in `{text}`")
        out.append(int(hex.decode(), 16))
        at += 3
    try:
        return out.decode()
    except UnicodeDecodeError:
        raise ScopeError(f"bad escape in `{text}`") from None


def fill(
    template: str,
    params: Mapping[str, str],
    escape: Callable[[str], str] = escape_id,
) -> str:
    """`template` with each `{name}` its parameter, escaped."""

    def value(match: re.Match[str]) -> str:
        name = match[1]
        if name not in params:
            raise KeyError(f"`{template}` needs `{name}`")
        return escape(params[name])

    return re.sub(r"\{([^}]+)\}", value, template)


@dataclass(frozen=True)
class Segment:
    """One step of a path: a resource (`wallets`), or narrowed to one id
    (`wallets[a]`)."""

    name: str
    id: str | None = None

    @classmethod
    def parse(cls, text: str) -> Segment:
        name, bracket, rest = text.partition("[")
        if not bracket:
            if not _is_name(text):
                raise ScopeError(f"`{text}` is not a name")
            return cls(text)
        if not rest.endswith("]"):
            raise ScopeError(f"bad id in `{name}`")
        id = _unescape(rest[:-1])
        if not id:
            raise ScopeError(f"bad id in `{name}`")
        if not _is_name(name):
            raise ScopeError(f"`{name}` is not a name")
        return cls(name, id)

    def covers(self, other: Segment) -> bool:
        return self.name == other.name and self.id in (None, other.id)

    def _key(self) -> tuple[object, ...]:
        """Rust's order: its name, then no id before an id."""
        return (self.name, (0,) if self.id is None else (1, self.id))

    def __str__(self) -> str:
        if self.id is None:
            return self.name
        return f"{self.name}[{escape_id(self.id)}]"


@dataclass(frozen=True)
class Scope:
    """One scope: a path, full or read only."""

    path: tuple[Segment, ...]
    read: bool = False

    @classmethod
    def parse(cls, text: str) -> Scope:
        """`text`: steps split by `.` (not inside `[` `]`), then `:read` when
        read only."""
        steps: list[str] = []
        start, inside, end = 0, False, len(text)
        for at, char in enumerate(text):
            if char == "[" and not inside:
                inside = True
            elif char == "]" and inside:
                inside = False
            elif char == "." and not inside:
                steps.append(text[start:at])
                start = at + 1
            elif char == ":" and not inside:
                end = at
                break
        if inside:
            raise ScopeError(f"bad id in `{text}`")
        if text[end:] not in ("", _READ):
            raise ScopeError(f"`{text}` does not end with `{_READ}`")
        if not text:
            raise ScopeError("a scope needs a resource")
        steps.append(text[start:end])
        return cls(tuple(Segment.parse(step) for step in steps), end < len(text))

    def covers(self, other: Scope) -> bool:
        """Whether it allows everything `other` allows: it is full or `other`
        read only, and its path is `other`'s or above it."""
        return (not self.read or other.read) and self._reaches(other.path)

    def _reaches(self, path: tuple[Segment, ...]) -> bool:
        return len(self.path) <= len(path) and all(
            ours.covers(theirs) for ours, theirs in zip(self.path, path)
        )

    def _key(self) -> tuple[object, ...]:
        return (tuple(step._key() for step in self.path), self.read)

    def __str__(self) -> str:
        return ".".join(str(step) for step in self.path) + (_READ if self.read else "")


class Scopes:
    """A set of scopes a key or a session holds; its text is theirs,
    space-separated."""

    __slots__ = ("_items",)

    def __init__(self, scopes: Iterable[Scope] = ()) -> None:
        self._items = tuple(scopes)

    @classmethod
    def parse(cls, value: str | Packed) -> Scopes:
        """Space-separated text (empty: none), or a packed claim."""
        if isinstance(value, str):
            return cls(Scope.parse(text) for text in _WHITESPACE.split(value) if text)
        return cls(
            Scope.parse(f"{prefix}.{text}" if prefix else text)
            for prefix in sorted(value)
            for text in value[prefix]
        )

    def covers(self, scope: Scope | str) -> bool:
        """Whether one of them covers `scope`; text that does not parse is
        never covered."""
        if isinstance(scope, str):
            try:
                scope = Scope.parse(scope)
            except ScopeError:
                return False
        return any(held.covers(scope) for held in self._items)

    def first_uncovered(self, others: Scopes) -> Scope | None:
        """The first of `others` none of them covers; None when they cover
        every one."""
        return next((scope for scope in others if not self.covers(scope)), None)

    def expand(self, depends: Mapping[str, Sequence[str]]) -> Scopes:
        """Them and every scope `depends` adds, to a fixed point, compressed.
        `depends` maps a source path (`com.hashinbox.triggers`) to the
        scopes holding it needs. A held scope triggers a source when it
        covers the source or an id of it, and adds each target whole: read
        only when the target is written `:read` or the held scope is read
        only. An entry whose text does not parse adds nothing."""
        rules: list[tuple[Scope, list[Scope]]] = []
        for text, texts in depends.items():
            try:
                rules.append((Scope.parse(text), [Scope.parse(t) for t in texts]))
            except ScopeError:
                continue
        held = list(self._items)
        # A list's iterator also yields what is appended while iterating.
        for scope in held:
            for source, targets in rules:
                if not _triggers(scope, source):
                    continue
                for target in targets:
                    added = Scope(target.path, target.read or scope.read)
                    if not any(kept.covers(added) for kept in held):
                        held.append(added)
        return Scopes(held).compress()

    def compress(self) -> Scopes:
        """The fewest scopes allowing the same, sorted."""
        items = self._items

        def dropped(at: int) -> bool:
            scope = items[at]
            return any(
                other != at
                and held.covers(scope)
                and (not scope.covers(held) or other < at)
                for other, held in enumerate(items)
            )

        kept = [scope for at, scope in enumerate(items) if not dropped(at)]
        return Scopes(sorted(kept, key=Scope._key))

    def pack(self) -> dict[str, list[str]]:
        """Compressed, then grouped by their shared prefix (the leading
        plain names every scope starting with the same step shares, each
        keeping one step at least): the JWT claim."""
        groups: dict[Segment, list[Scope]] = {}
        for scope in self.compress():
            groups.setdefault(scope.path[0], []).append(scope)
        packed: dict[str, list[str]] = {}
        for scopes in groups.values():
            shared = _shared_prefix(scopes)
            for scope in scopes:
                prefix = ".".join(str(step) for step in scope.path[:shared])
                rest = Scope(scope.path[shared:], scope.read)
                packed.setdefault(prefix, []).append(str(rest))
        return dict(sorted(packed.items()))

    def __iter__(self) -> Iterator[Scope]:
        return iter(self._items)

    def __len__(self) -> int:
        return len(self._items)

    def __eq__(self, other: object) -> bool:
        return isinstance(other, Scopes) and self._items == other._items

    def __hash__(self) -> int:
        return hash(self._items)

    def __str__(self) -> str:
        return " ".join(str(scope) for scope in self._items)

    def __repr__(self) -> str:
        return f"Scopes.parse({str(self)!r})"


def _triggers(held: Scope, source: Scope) -> bool:
    """Whether `held` triggers `source`'s dependencies: it covers `source`,
    its ids ignored."""
    path = tuple(Segment(step.name) for step in held.path)
    return Scope(path)._reaches(source.path)


def _shared_prefix(scopes: list[Scope]) -> int:
    """How many leading steps of `scopes` are the same plain name in each,
    each scope keeping one step at least."""
    shortest = min(len(scope.path) for scope in scopes)
    first = scopes[0].path
    count = 0
    for at in range(shortest - 1):
        step = first[at]
        if step.id is not None or any(scope.path[at] != step for scope in scopes):
            break
        count += 1
    return count

"""Hand-written: packages/scope ported. A scope is text
(`com.hashinbox.wallets[w1].$read`): a path of resources and an action. One
covers another, a set packs into the JWT `scopes` claim, and a template's
`{name}` holes are filled with escaped ids. The vectors in
`apps/server/clients/_golden/scope.json` keep this port honest."""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable, Iterator, Mapping
from dataclasses import dataclass
from typing import TypeAlias

__all__ = [
    "ANY",
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

# `*`: any resource (a segment's name) or any action.
ANY = "*"

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
    """One step of a path: a resource (`wallets`), narrowed to one id
    (`wallets[a]`), or `*` (name `ANY`)."""

    name: str
    id: str | None = None

    @classmethod
    def parse(cls, text: str) -> Segment:
        if text == ANY:
            return cls(ANY)
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
        if self.name == ANY:
            return True
        if other.name == ANY:
            return False
        return self.name == other.name and self.id in (None, other.id)

    def _key(self) -> tuple[object, ...]:
        """Rust's order: a name (then no id before an id) before `*`."""
        if self.name == ANY:
            return (1,)
        return (0, self.name, (0,) if self.id is None else (1, self.id))

    def __str__(self) -> str:
        if self.id is None:
            return self.name
        return f"{self.name}[{escape_id(self.id)}]"


@dataclass(frozen=True)
class Scope:
    """One scope: a path and an action (`ANY` for `$*`)."""

    path: tuple[Segment, ...]
    action: str

    @classmethod
    def parse(cls, text: str) -> Scope:
        """`text`: steps split by `.` (not inside `[` `]`), the last one the
        action, `$name` or `$*`."""
        steps: list[str] = []
        start, inside = 0, False
        for at, char in enumerate(text):
            if char == "[" and not inside:
                inside = True
            elif char == "]" and inside:
                inside = False
            elif char == "." and not inside:
                steps.append(text[start:at])
                start = at + 1
        if inside:
            raise ScopeError(f"bad id in `{text}`")
        last = text[start:]
        if not last.startswith("$"):
            raise ScopeError(f"`{text}` does not end with `$action`")
        action = last[1:]
        if action != ANY and not _is_name(action):
            raise ScopeError(f"`{action}` is not a name")
        path = tuple(Segment.parse(step) for step in steps)
        if not path:
            raise ScopeError("a scope needs a resource")
        return cls(path, action)

    def covers(self, other: Scope) -> bool:
        """Whether it allows everything `other` allows: its action the same
        or `$*`, its path matching step by step (`*` in the middle: any one
        step; at the end: anything below, itself included)."""
        if self.action != ANY and self.action != other.action:
            return False
        for at, step in enumerate(self.path):
            if step.name == ANY and at == len(self.path) - 1:
                return True
            if at >= len(other.path) or not step.covers(other.path[at]):
                return False
        return len(self.path) == len(other.path)

    def _key(self) -> tuple[object, ...]:
        action = (1,) if self.action == ANY else (0, self.action)
        return (tuple(step._key() for step in self.path), action)

    def __str__(self) -> str:
        return "".join(f"{step}." for step in self.path) + f"${self.action}"


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
                rest = Scope(scope.path[shared:], scope.action)
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


def _shared_prefix(scopes: list[Scope]) -> int:
    """How many leading steps of `scopes` are the same plain name in each,
    each scope keeping one step at least."""
    shortest = min(len(scope.path) for scope in scopes)
    first = scopes[0].path
    count = 0
    for at in range(shortest - 1):
        step = first[at]
        plain = step.name != ANY and step.id is None
        if not plain or any(scope.path[at] != step for scope in scopes):
            break
        count += 1
    return count

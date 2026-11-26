from typing import Any

import pytest

from hashinbox import Scope, ScopeError, Scopes, escape_id, fill

from .golden import load

VECTORS = load("scope.json")


@pytest.mark.parametrize("case", VECTORS["parse"])
def test_parse(case: dict[str, Any]) -> None:
    assert str(Scope.parse(case["text"])) == case["display"]


@pytest.mark.parametrize("text", VECTORS["refuse"])
def test_refuse(text: str) -> None:
    with pytest.raises(ScopeError):
        Scope.parse(text)


@pytest.mark.parametrize("case", VECTORS["covers"])
def test_covers(case: dict[str, Any]) -> None:
    ours, theirs = Scope.parse(case["ours"]), Scope.parse(case["theirs"])
    assert ours.covers(theirs) is case["covers"]


@pytest.mark.parametrize("case", VECTORS["escape"])
def test_escape(case: dict[str, str]) -> None:
    assert escape_id(case["id"]) == case["text"]
    assert fill("a[{id}].$read", {"id": case["id"]}) == f"a[{case['text']}].$read"


@pytest.mark.parametrize("case", VECTORS["first_uncovered"])
def test_first_uncovered(case: dict[str, Any]) -> None:
    granted, asked = Scopes.parse(case["granted"]), Scopes.parse(case["asked"])
    first = granted.first_uncovered(asked)
    assert (None if first is None else str(first)) == case["first_uncovered"]


@pytest.mark.parametrize("case", VECTORS["sets"])
def test_sets(case: dict[str, Any]) -> None:
    scopes = Scopes.parse(case["scopes"])
    assert str(scopes) == case["display"]
    assert str(scopes.compress()) == case["compressed"]
    assert scopes.pack() == case["packed"]
    assert Scopes.parse(case["packed"]) == scopes.compress()

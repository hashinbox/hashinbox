from typing import Any

import pytest

from hashinbox import WALLET_GET, Amr, Scopes, Secret, Session

from .golden import load

VECTORS = load("session.json")


def secret() -> Secret:
    return Secret.parse(VECTORS["id"], VECTORS["secret"])


@pytest.mark.parametrize("case", VECTORS["sessions"])
def test_signing_is_byte_identical(case: dict[str, Any]) -> None:
    session = secret().issue(
        case["scopes"],
        subject=case["sub"],
        ttl=case["exp"] - case["iat"],
        issued_at=case["iat"],
    )
    assert session.token == case["token"]
    assert session.issuer == VECTORS["id"]
    assert session.subject == case["sub"]
    assert session.scopes == Scopes.parse(case["scopes"]).compress()


def test_a_server_made_token_parses() -> None:
    user = VECTORS["user"]
    session = Session.parse(user["token"])
    assert (session.issuer, session.subject) == (user["iss"], user["sub"])
    assert (session.issued_at, session.expires_at) == (user["iat"], user["exp"])
    assert str(session.scopes) == user["scopes"]
    assert session.amr == (Amr.PWD, Amr.OTP)
    assert session.expired(now=user["exp"])
    assert not session.expired(now=user["exp"] - 1)
    assert session.can(WALLET_GET, {"id": "a] b"})


def test_a_session_checks_its_scopes() -> None:
    session = secret().issue("com.hashinbox.wallets[x].$read")
    assert session.can(WALLET_GET, {"id": "x"})
    assert not session.can(WALLET_GET, {"id": "y"})
    assert 0 < session.expires_at - session.issued_at <= 600


def test_a_mistyped_secret_is_refused() -> None:
    value = VECTORS["secret"]
    typo = value[:10] + ("a" if value[10] != "a" else "b") + value[11:]
    with pytest.raises(ValueError):
        Secret.parse(VECTORS["id"], typo)
    with pytest.raises(ValueError):
        Secret.parse(VECTORS["id"], "his/0OIl")
    with pytest.raises(ValueError):
        Secret.parse("key", value)
    assert VECTORS["secret"] not in repr(secret())


def test_a_ttl_past_a_day_is_refused() -> None:
    with pytest.raises(ValueError):
        secret().issue(ttl=24 * 60 * 60 + 1)

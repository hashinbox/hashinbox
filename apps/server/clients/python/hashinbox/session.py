"""Hand-written: a session JWT (design 096), decoded but not verified: only
its issuer can check it, and the server does."""

from __future__ import annotations

import base64
import json
import time
from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from .models import Amr
from .scope import Scopes, fill

if TYPE_CHECKING:
    from .client import Operation

__all__ = ["Session"]


@dataclass(frozen=True, kw_only=True)
class Session:
    """A session: a JWT and the claims it carries."""

    # The JWT, sent as `Authorization: Bearer …`.
    token: str = field(repr=False)
    # `iss` (= the header's `kid`): `user/{id}`, or a key's `hip/…`.
    issuer: str
    # `sub`: a user's id, or who a key's session is for.
    subject: str | None
    # What it may do, unpacked.
    scopes: Scopes
    # `iat`, Unix seconds.
    issued_at: int
    # `exp`, Unix seconds.
    expires_at: int
    # How its login proved who it is; empty for a key's.
    amr: tuple[Amr, ...] = ()

    @classmethod
    def parse(cls, token: str) -> Session:
        """The session `token` is: its claims decoded, its signature not
        checked. A ValueError when it is not a session JWT."""
        parts = token.split(".")
        if len(parts) != 3:
            raise ValueError("not a JWT")
        header, claims = _part(parts[0]), _part(parts[1])
        if header.get("alg") != "HS256":
            raise ValueError("its alg is not HS256")
        issuer = claims.get("iss")
        if not isinstance(issuer, str) or header.get("kid") != issuer:
            raise ValueError("its header does not name its issuer")
        iat, exp = claims.get("iat"), claims.get("exp")
        if type(iat) is not int or type(exp) is not int:
            raise ValueError("it has no times")
        subject = claims.get("sub")
        return cls(
            token=token,
            issuer=issuer,
            subject=subject if isinstance(subject, str) else None,
            scopes=Scopes.parse(claims.get("scopes") or {}),
            issued_at=iat,
            expires_at=exp,
            amr=tuple(Amr(method) for method in claims.get("amr") or ()),
        )

    def expired(self, now: float | None = None) -> bool:
        """Whether it has expired at `now` (Unix seconds; the clock's by
        default)."""
        return self.expires_at <= (time.time() if now is None else now)

    def can(self, operation: Operation, params: Mapping[str, str] = {}) -> bool:
        """Whether it holds every scope `operation` needs for `params` (its
        path parameters)."""
        return all(
            self.scopes.covers(fill(template, params))
            for template in operation.templates
        )


def _part(part: str) -> dict[str, Any]:
    """A JWT part's JSON object."""
    try:
        value = json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
    except ValueError:
        raise ValueError("not a JWT") from None
    if isinstance(value, dict):
        return value
    raise ValueError("not a JWT")

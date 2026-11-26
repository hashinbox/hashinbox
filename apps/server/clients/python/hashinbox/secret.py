"""Hand-written: a key's secret (design 096) and the sessions it signs. The
secret never goes over the wire: only the JWTs it signs do.

Signing needs this machine's clock roughly right: the server refuses a
session whose `iat` is more than 60 s ahead of its own clock, or whose `exp`
is past."""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import time
import zlib

from .scope import Scopes
from .session import Session

__all__ = ["MAX_TTL", "Secret"]

# The longest a session lasts: 24 hours, in seconds.
MAX_TTL = 24 * 60 * 60

_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"


class Secret:
    """A key: its client id (`hip/…`) and its secret (`his/…`), the HS256 key
    of the sessions it signs. Its repr shows the id only."""

    __slots__ = ("_value", "id")

    def __init__(self, id: str, value: str) -> None:
        self.id = id
        self._value = value

    @classmethod
    def parse(cls, id: str, value: str) -> Secret:
        """The key `id` and `value` are; a ValueError when `value` is not a
        `his/…` secret or fails its checksum (a typo)."""
        if not id.startswith("hip/") or id == "hip/":
            raise ValueError("a key id is `hip/…`")
        raw = _base58(value[4:]) if value.startswith("his/") else None
        if raw is None or len(raw) != 36:
            raise ValueError("not a `his/…` secret")
        if zlib.crc32(raw[:32]) != int.from_bytes(raw[32:], "big"):
            raise ValueError("the secret's checksum does not match: a typo?")
        return cls(id, value)

    def issue(
        self,
        scopes: Scopes | str | None = None,
        subject: str | None = None,
        ttl: int = 600,
        issued_at: int | None = None,
    ) -> Session:
        """A session of the key, HS256-signed here (`iss` = `kid` = its id):
        `scopes` (none when None; the server refuses any the key does not
        cover), for `subject`, lasting `ttl` seconds (1 to `MAX_TTL`) from
        `issued_at` (the clock's by default)."""
        if not 0 < ttl <= MAX_TTL:
            raise ValueError(f"ttl is 1 to {MAX_TTL} seconds")
        held = scopes if isinstance(scopes, Scopes) else Scopes.parse(scopes or "")
        iat = int(time.time()) if issued_at is None else issued_at
        claims: dict[str, object] = {"iss": self.id}
        if subject is not None:
            claims["sub"] = subject
        claims |= {"iat": iat, "exp": iat + ttl, "scopes": held.pack()}
        header = {"typ": "JWT", "alg": "HS256", "kid": self.id}
        body = f"{_part(header)}.{_part(claims)}"
        mac = hmac.new(self._value.encode(), body.encode(), hashlib.sha256)
        return Session.parse(f"{body}.{_base64url(mac.digest())}")

    def __repr__(self) -> str:
        return f"Secret({self.id!r}, …)"


def _part(value: object) -> str:
    """`value` as a JWT part: compact JSON as serde_json writes it, base64url."""
    text = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    return _base64url(text.encode())


def _base64url(raw: bytes) -> str:
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()


def _base58(text: str) -> bytes | None:
    """The bytes base-58 `text` is, each leading `1` a zero byte; None for a
    character outside the alphabet."""
    number = 0
    for char in text:
        digit = _ALPHABET.find(char)
        if digit < 0:
            return None
        number = number * 58 + digit
    zeros = len(text) - len(text.lstrip("1"))
    return bytes(zeros) + number.to_bytes((number.bit_length() + 7) // 8, "big")

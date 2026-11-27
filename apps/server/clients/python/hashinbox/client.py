"""Hand-written: what the generated operations (operations.py) and models
(models.py) use, and the default `Client` over httpx. cligen (via
apps/server/build.rs) writes those two files beside this; it never touches
this file."""

from __future__ import annotations

import asyncio
import json
import time
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass
from types import TracebackType
from typing import Any, Generic, Protocol, Self, TypeVar
from urllib.parse import quote

import httpx

from .scope import Scopes, escape_id, fill

__all__ = [
    "ApiError",
    "Client",
    "Event",
    "Granted",
    "Operation",
    "RawEvent",
    "Reason",
    "Request",
    "Stream",
    "Streamer",
    "can",
    "path",
    "scopes",
]

T = TypeVar("T")
U = TypeVar("U")


@dataclass(frozen=True, kw_only=True)
class Operation:
    """An operation's static description."""

    id: str
    method: str
    # The path template: `/v1/wallets/{id}`.
    path: str
    # The scope templates.
    templates: tuple[str, ...] = ()


class Request(Protocol):
    """An operation's input, as each generated request dataclass gives it."""

    def operation(self) -> Operation: ...

    def params(self) -> dict[str, str]:
        """The path parameters by name."""
        return {}

    def query(self) -> list[tuple[str, str]]:
        """The query pairs."""
        return []

    def content(self) -> str | None:
        """The JSON body; None when none."""
        return None


class Granted(Protocol):
    """What holds scopes: a `Session`'s or a key's `Scopes`."""

    def covers(self, scope: str) -> bool: ...


@dataclass(frozen=True, kw_only=True)
class RawEvent:
    """One server-sent event, as read."""

    event: str
    data: str
    id: str | None = None


@dataclass(frozen=True)
class Event(Generic[T]):
    """One event of a stream, its data decoded."""

    event: str
    data: T
    id: str | None = None


class Streamer(Protocol):
    """Opens a request as a stream of events."""

    def open(self, request: Request) -> Stream[RawEvent]: ...


def path(request: Request) -> str:
    """The request's path, each `{name}` its parameter, URL-escaped."""
    params = request.params()
    return fill(request.operation().path, params, lambda value: quote(value, safe=""))


def scopes(request: Request) -> list[str]:
    """The scopes the request needs, each hole escaped."""
    templates = request.operation().templates
    return [fill(template, request.params(), escape_id) for template in templates]


def can(granted: Granted, request: Request) -> bool:
    """Whether `granted` holds every scope the request needs."""
    return all(granted.covers(scope) for scope in scopes(request))


def text(value: object) -> str:
    """`value` as path or query text."""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def encode(value: Any) -> Any:
    """`value` as JSON: models through their `to_json`."""
    if hasattr(value, "to_json"):
        return value.to_json()
    if isinstance(value, list):
        return [encode(item) for item in value]
    if isinstance(value, dict):
        return {key: encode(item) for key, item in value.items()}
    return value


def dump(value: Any) -> str:
    """`value` as JSON text."""
    return json.dumps(encode(value))


def data(raw: RawEvent) -> Any:
    """A raw event's data, decoded."""
    return json.loads(raw.data)


def optional(decode: Callable[[Any], T], value: Any) -> T | None:
    """`value` decoded, unless None."""
    return None if value is None else decode(value)


def array(decode: Callable[[Any], T], value: Any) -> list[T]:
    """Each item of `value` decoded."""
    return [decode(item) for item in value]


def mapping(decode: Callable[[Any], T], value: Any) -> dict[str, T]:
    """Each value of `value` decoded."""
    return {key: decode(item) for key, item in value.items()}


def first(value: Any, *decoders: Callable[[Any], object]) -> Any:
    """`value` decoded by the first of `decoders` that takes it."""
    for decode in decoders:
        try:
            return decode(value)
        except (KeyError, TypeError, ValueError, AttributeError):
            continue
    raise ValueError("no variant fits")


# A connect of a `Stream`: the request sent anew, with the last event id
# read, if any, as its `cursor`; its 2xx streaming response.
Connect = Callable[[str | None], Awaitable[httpx.Response]]


class Stream(Generic[T]):
    """A stream's events across the server's windows: an async iterator, and
    `async with` or `await stop()` to end it. The server ends each answer
    after its window (15 s) with `event: close`; the stream then reconnects
    at once, sending the last event id read (`cursor`) as the query's
    `cursor` (the caller's own, if any, on the first connect), and goes on. An answer that ends without `close` ends
    the stream: it is done. A failed connect (a transport error, a 5xx or
    429) or an answer cut short reconnects after `backoff` seconds, doubled
    each time up to `max_backoff`, reset by a 2xx; any other non-2xx raises
    its `ApiError`. Until `stop()`, it keeps reconnecting."""

    def __init__(
        self,
        connect: Connect,
        decode: Callable[[RawEvent], T | None],
        *,
        backoff: float = 0.5,
        max_backoff: float = 8.0,
    ) -> None:
        self._connect = connect
        self._decode = decode
        self.backoff = backoff
        self.max_backoff = max_backoff
        # The id of the last event read, sent as `?cursor=` on reconnecting.
        self.cursor: str | None = None
        self._stopped = asyncio.Event()
        self._response: httpx.Response | None = None

    def map(self, decode: Callable[[T], U | None]) -> Stream[U]:
        """Its events decoded by `decode`, each it answers None for skipped."""
        own = self._decode

        def both(raw: RawEvent) -> U | None:
            value = own(raw)
            return None if value is None else decode(value)

        return Stream(
            self._connect, both, backoff=self.backoff, max_backoff=self.max_backoff
        )

    async def stop(self) -> None:
        """Ends it for good: no reconnect, the current answer closed."""
        self._stopped.set()
        if self._response is not None:
            await self._response.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        trace: TracebackType | None,
    ) -> None:
        await self.stop()

    async def __aiter__(self) -> AsyncIterator[T]:
        delay = self.backoff
        while not self._stopped.is_set():
            try:
                response = await self._connect(self.cursor)
            except ApiError as error:
                if error.status < 500 and error.status != 429:
                    raise
            except httpx.TransportError:
                pass
            else:
                delay = self.backoff
                self._response = response
                try:
                    async for raw in _events(response.aiter_lines()):
                        if raw.id:
                            self.cursor = raw.id
                        if raw.event == "close":
                            break
                        value = self._decode(raw)
                        if value is not None:
                            yield value
                        if self._stopped.is_set():
                            return
                    else:
                        return
                    continue
                except (httpx.TransportError, httpx.StreamError):
                    pass
                finally:
                    self._response = None
                    await response.aclose()
            if self._stopped.is_set():
                return
            try:
                await asyncio.wait_for(self._stopped.wait(), delay)
            except TimeoutError:
                pass
            delay = min(delay * 2, self.max_backoff)


def _raw(raw: RawEvent) -> RawEvent:
    return raw


# The models import the helpers above from this module, so these come after.
from .models import CreateSessionRequest, IssuedSession, Kind
from .secret import Secret
from .session import Session


class Reason:
    """The fixed machine strings of `ApiError.reason`: what a client acts on."""

    # A login's email, password or two-factor code is wrong, or the current
    # password or the code of a two-factor change (401).
    WRONG_CREDENTIALS = "wrong_credentials"
    # The password was right and the user's two-factor code is needed: ask
    # it, post again (401).
    TOTP_REQUIRED = "totp_required"
    # A genuine session (or key) no longer accepted: expired, or invalidated
    # since it was issued (401). Log in again.
    SESSION_EXPIRED = "session_expired"
    # The caller lacks a scope; metadata `scope` names it (403).
    MISSING_SCOPE = "missing_scope"


class ApiError(Exception):
    """Any failure as the API's error: `kind`, `message`, `reason` (what a
    client acts on: `wrong_credentials`, `totp_required`,
    `session_expired`, `missing_scope`), `metadata`, and the answer's
    `status` (0 when none came). A plain value; `of` turns anything into
    one."""

    def __init__(
        self,
        message: str,
        *,
        kind: Kind = Kind.UNAVAILABLE,
        reason: str | None = None,
        metadata: dict[str, str] | None = None,
        status: int = 0,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.kind = kind
        self.message = message
        self.reason = reason
        self.metadata = metadata or {}

    @classmethod
    def of(cls, error: object) -> ApiError:
        """Anything as an `ApiError`: itself when one; an answer (an
        `httpx.Response`, read) in the API's error shape that error, with
        its status; any other answer (an HTML 502 from a proxy, an empty
        body, other JSON, as `health_get`'s 503 `Report`) `unavailable`,
        its status, its text's first 1024 characters as
        `metadata["response"]`; an exception `unavailable`, its message,
        itself as `__cause__`; anything else `unavailable`, `str` of it."""
        if isinstance(error, ApiError):
            return error
        if isinstance(error, httpx.Response):
            return cls._answer(error.status_code, error.text)
        wrapped = cls(str(error))
        if isinstance(error, BaseException):
            wrapped.__cause__ = error
        return wrapped

    @classmethod
    def _answer(cls, status: int, text: str) -> ApiError:
        """The error a `status` answer with body `text` is."""
        try:
            body: Any = json.loads(text)
        except ValueError:
            body = None
        error = body if isinstance(body, dict) else {}
        kind, message = error.get("kind"), error.get("message")
        reason, metadata = error.get("reason"), error.get("metadata")
        if (
            kind not in list(Kind)
            or not isinstance(message, str)
            or not isinstance(reason, str | None)
            or not isinstance(metadata, dict | None)
        ):
            return cls(
                f"could not parse the response (HTTP {status})",
                metadata={"response": text[:1024]},
                status=status,
            )
        return cls(
            message, kind=Kind(kind), reason=reason, metadata=metadata, status=status
        )


def missing_scope(operation: str, scope: str) -> ApiError:
    """The server's 403 for a call `operation` lacking `scope`, refused
    here, unsent."""
    return ApiError(
        f"{operation} needs scope {scope}",
        kind=Kind.PERMISSION_DENIED,
        reason=Reason.MISSING_SCOPE,
        metadata={"scope": scope, "operation": operation},
        status=403,
    )


# How long before its `exp` a self-signed session is replaced, seconds.
REFRESH = 60


class Client:
    """The hashinbox API over httpx: the `Client` and `Streamer` of every
    generated operation, with the session it sends.

    - `session`: a JWT session to send (a login's, or one a backend issued).
    - `secret`: a key; the client signs its own sessions with it (the secret
      never goes over the wire): `scopes`, or the key's own (read once),
      lasting `ttl` seconds, signed anew within 60 s of expiry or after a
      401 `session_expired` (retried once). This machine's clock must be
      within 60 s of the server's.
    - `http`: an httpx.AsyncClient to send with (its transport, proxies);
      not closed by this one.
    - `check_scopes`: refuse locally, unsent, a call the session's scopes
      do not cover: the server's 403 `missing_scope` `ApiError`.
    - `backoff`, `max_backoff`: a stream's waits, seconds, before it
      reconnects after a failure (see `Stream`).
    """

    def __init__(
        self,
        base_url: str,
        *,
        session: Session | None = None,
        secret: Secret | None = None,
        scopes: Scopes | str | None = None,
        ttl: int = 600,
        http: httpx.AsyncClient | None = None,
        timeout: float = 30.0,
        check_scopes: bool = True,
        backoff: float = 0.5,
        max_backoff: float = 8.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self._session = session
        self._secret = secret
        self._scopes = Scopes.parse(scopes) if isinstance(scopes, str) else scopes
        self._ttl = ttl
        self._owned = http is None
        self._http = http or httpx.AsyncClient()
        self._timeout = timeout
        self._check_scopes = check_scopes
        self._signing = asyncio.Lock()
        self._backoff = (backoff, max_backoff)

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        trace: TracebackType | None,
    ) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        """Closes its httpx client, when it made it."""
        if self._owned:
            await self._http.aclose()

    @property
    def session(self) -> Session | None:
        """The session sent."""
        return self._session

    @session.setter
    def session(self, session: Session | None) -> None:
        self._session = session

    async def send(self, request: Request) -> Any:
        """`request` sent with the session: its 2xx body decoded (None when
        empty), else an `ApiError`."""
        response = await self._exchange(request)
        return response.json() if response.content else None

    def open(self, request: Request) -> Stream[RawEvent]:
        """`request`'s server-sent events, as read, reconnecting each window
        until it is done or stopped (see `Stream`)."""

        def connect(cursor: str | None) -> Awaitable[httpx.Response]:
            return self._exchange(request, stream=True, cursor=cursor)

        backoff, max_backoff = self._backoff
        return Stream(connect, _raw, backoff=backoff, max_backoff=max_backoff)

    def stream(self, request: Request) -> Stream[Event[Any]]:
        """`request`'s server-sent events, their data decoded from JSON."""
        return self.open(request).map(lambda raw: Event(raw.event, data(raw), raw.id))

    async def login(
        self,
        email: str,
        password: str | None = None,
        passphrase: str | None = None,
        totp: str | None = None,
    ) -> IssuedSession:
        """Logs in (no `Authorization` sent) and keeps the session. A wrong
        email, password, passphrase or code raises an `ApiError` with reason
        `wrong_credentials`; a missing code, `totp_required` (ask it, call
        again with `totp`)."""
        from .operations import SessionCreateRequest

        body = CreateSessionRequest(
            email=email, password=password, passphrase=passphrase, totp=totp
        )
        request = SessionCreateRequest(body=body)
        response = await self._exchange(request, anonymous=True)
        issued = IssuedSession.from_json(response.json())
        self._session = Session.parse(issued.token)
        return issued

    async def logout(self, all: bool = False) -> None:
        """Forgets the session; with `all`, first has the server refuse every
        session its issuer issued until now."""
        from .operations import SessionDeleteRequest

        try:
            if all:
                await self.send(SessionDeleteRequest(all=True))
        finally:
            self._session = None

    async def _exchange(
        self,
        request: Request,
        *,
        stream: bool = False,
        anonymous: bool = False,
        cursor: str | None = None,
    ) -> httpx.Response:
        """`request` sent with the session (signed anew once on a
        `session_expired` when a secret signs them): its 2xx response, else
        its `ApiError` raised."""
        retry = self._secret is not None and not anonymous
        while True:
            session = None if anonymous else await self._current()
            if session is not None and self._check_scopes:
                for scope in scopes(request):
                    if not session.scopes.covers(scope):
                        raise missing_scope(request.operation().id, scope)
            response = await self._fetch(request, session, stream, cursor)
            if response.is_success:
                return response
            if stream:
                await response.aread()
                await response.aclose()
            error = ApiError.of(response)
            if retry and error.status == 401 and error.reason == "session_expired":
                retry = False
                self._session = None
                continue
            raise error

    async def _fetch(
        self,
        request: Request,
        session: Session | None,
        stream: bool = False,
        cursor: str | None = None,
    ) -> httpx.Response:
        """One request, its response once it answers; with `cursor`, that as
        its query's `cursor` (in place of its own). A stream's reads wait as
        long as it takes."""
        headers = {"Accept": "text/event-stream" if stream else "application/json"}
        query = request.query()
        if cursor is not None:
            query = [pair for pair in query if pair[0] != "cursor"]
            query.append(("cursor", cursor))
        if session is not None:
            headers["Authorization"] = f"Bearer {session.token}"
        content = request.content()
        if content is not None:
            headers["Content-Type"] = "application/json"
        timeout = httpx.Timeout(self._timeout, read=None if stream else self._timeout)
        built = self._http.build_request(
            request.operation().method,
            self.base_url + path(request),
            params=tuple(query),
            headers=headers,
            content=content.encode() if content is not None else None,
            timeout=timeout,
        )
        return await self._http.send(built, stream=stream)

    async def _current(self) -> Session | None:
        """The session to send: the one kept, or one the secret signs when it
        lapses within a minute."""
        secret = self._secret
        if secret is None:
            return self._session
        async with self._signing:
            kept = self._session
            if kept is None or kept.expires_at - time.time() <= REFRESH:
                kept = self._session = await self._sign(secret)
            return kept

    async def _sign(self, secret: Secret) -> Session:
        """A new session of `secret`: the scopes asked, or the key's own,
        read once with a scopeless session."""
        from .operations import SessionGetRequest

        if self._scopes is None:
            probe = secret.issue(ttl=REFRESH)
            response = await self._fetch(SessionGetRequest(), probe)
            if not response.is_success:
                raise ApiError.of(response)
            # Read raw: `SessionRead.kind` decodes as the error `Kind`.
            self._scopes = Scopes.parse(response.json()["key"]["scopes"])
        return secret.issue(self._scopes, ttl=self._ttl)


async def _events(lines: AsyncIterator[str]) -> AsyncIterator[RawEvent]:
    """The events of a `text/event-stream` body's lines."""
    event: str | None = None
    id: str | None = None
    buffer: list[str] = []
    async for line in lines:
        if not line:
            if buffer:
                yield RawEvent(event=event or "message", data="\n".join(buffer), id=id)
            event, buffer = None, []
            continue
        name, _, value = line.partition(":")
        if not name:
            continue
        value = value.removeprefix(" ")
        if name == "event":
            event = value
        elif name == "data":
            buffer.append(value)
        elif name == "id":
            id = value

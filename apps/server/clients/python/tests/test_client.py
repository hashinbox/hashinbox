import itertools
import json
import time
from collections.abc import Callable
from typing import Any

import httpx
import pytest

from hashinbox import (
    ApiError,
    Client,
    CreateWalletRequest,
    Kind,
    LogsStreamRequest,
    Reason,
    Secret,
    Session,
    WalletGetRequest,
    WalletsCreateRequest,
    WalletsListRequest,
    logs_stream,
    wallet_get,
    wallets_create,
)

from .golden import load

VECTORS = load("session.json")
SECRET = Secret.parse(VECTORS["id"], VECTORS["secret"])
WALLET: dict[str, Any] = {
    "id": "ethereum.mainnet.0xabc",
    "chain": "ethereum",
    "network": "mainnet",
    "address": "0xabc",
    "external_id": None,
    "indexes": {},
    "keyed": True,
    "tags": [],
}

Handler = Callable[[httpx.Request], httpx.Response]


def client(handler: Handler, **options: object) -> Client:
    http = httpx.AsyncClient(transport=httpx.MockTransport(handler))
    return Client("http://hashinbox.test/", http=http, **options)  # type: ignore[arg-type]


def bearer(request: httpx.Request) -> Session:
    return Session.parse(request.headers["Authorization"].removeprefix("Bearer "))


async def test_path_query_body_and_bearer() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        if request.method == "POST":
            return httpx.Response(201, json=WALLET)
        return httpx.Response(200, json={"items": [], "next_cursor": None})

    session = SECRET.issue("com.hashinbox.wallets")
    async with client(handler, session=session) as api:
        body = CreateWalletRequest(chain="ethereum", network="mainnet")
        await wallets_create(api, WalletsCreateRequest(body=body))
        await api.send(WalletsListRequest(limit=5))
    post, get = seen
    assert post.url.path == "/v1/wallets"
    assert json.loads(post.content) == {"chain": "ethereum", "network": "mainnet"}
    assert post.headers["Content-Type"] == "application/json"
    assert post.headers["Authorization"] == f"Bearer {session.token}"
    assert get.url.params["limit"] == "5"


async def test_a_path_parameter_is_escaped() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, json=WALLET)

    session = SECRET.issue("com.hashinbox.wallets:read")
    async with client(handler, session=session) as api:
        wallet = await wallet_get(api, WalletGetRequest(id="a/b c"))
    assert wallet.address == "0xabc"
    assert seen[0].url.raw_path == b"/v1/wallets/a%2Fb%20c"


async def test_an_error_is_mapped() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        error = {
            "kind": "permission_denied",
            "message": "missing scope",
            "reason": "missing_scope",
            "metadata": {"scope": "com.hashinbox.wallets:read"},
        }
        return httpx.Response(403, json=error)

    async with client(handler, check_scopes=False) as api:
        with pytest.raises(ApiError) as raised:
            await api.send(WalletGetRequest(id="x"))
    error = raised.value
    assert (error.status, error.kind, error.reason) == (
        403,
        Kind.PERMISSION_DENIED,
        "missing_scope",
    )
    assert error.metadata == {"scope": "com.hashinbox.wallets:read"}


async def test_a_missing_scope_is_refused_locally() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        raise AssertionError("sent")

    session = SECRET.issue("com.hashinbox.wallets[x]:read")
    async with client(handler, session=session) as api:
        with pytest.raises(ApiError) as raised:
            await api.send(WalletGetRequest(id="y"))
    error = raised.value
    scope = "com.hashinbox.wallets[y]:read"
    assert (error.status, error.kind, error.reason) == (
        403,
        Kind.PERMISSION_DENIED,
        Reason.MISSING_SCOPE,
    )
    assert error.message == f"wallet_get needs scope {scope}"
    assert error.metadata == {"scope": scope, "operation": "wallet_get"}


async def test_a_secret_signs_its_own_sessions_and_re_signs_on_expiry() -> None:
    sent: list[Session] = []

    def handler(request: httpx.Request) -> httpx.Response:
        session = bearer(request)
        assert VECTORS["secret"] not in request.headers["Authorization"]
        if request.url.path == "/v1/session":
            key = {"scopes": {"com.hashinbox": ["wallets:read"]}}
            return httpx.Response(200, json={"key": key})
        sent.append(session)
        if len(sent) == 1:
            error = {"kind": "unauthenticated", "message": "expired"}
            return httpx.Response(401, json=error | {"reason": "session_expired"})
        return httpx.Response(200, json=WALLET)

    async with client(handler, secret=SECRET) as api:
        await api.send(WalletGetRequest(id="x"))
        first, second = sent
        assert str(first.scopes) == "com.hashinbox.jobs:read com.hashinbox.wallets:read"
        assert first.expires_at - first.issued_at == 600
        assert api.session == second
        # Within a minute of its expiry, a new one is signed.
        api.session = SECRET.issue("com.hashinbox.wallets:read", ttl=30)
        await api.send(WalletGetRequest(id="x"))
        assert sent[2].expires_at > time.time() + 500


async def test_login_keeps_the_session_and_surfaces_totp_required() -> None:
    user = VECTORS["user"]
    asked: list[dict[str, str]] = []

    def handler(request: httpx.Request) -> httpx.Response:
        assert "Authorization" not in request.headers
        body = json.loads(request.content)
        asked.append(body)
        if "totp" not in body:
            error = {"kind": "unauthenticated", "message": "code needed"}
            return httpx.Response(401, json=error | {"reason": "totp_required"})
        issued = {"token": user["token"], "expires_at": user["exp"], "scopes": {}}
        return httpx.Response(201, json=issued)

    async with client(handler) as api:
        with pytest.raises(ApiError) as raised:
            await api.login("a@b.c", password="pw")
        assert raised.value.reason == "totp_required"
        await api.login("a@b.c", password="pw", totp="123456")
        assert api.session is not None and api.session.issuer == "user/0193a"
        await api.logout()
        assert api.session is None
    assert asked[1] == {"email": "a@b.c", "password": "pw", "totp": "123456"}


async def test_a_stream_yields_its_events() -> None:
    body = (
        b": keep-alive\n\n"
        b'data: {"id": 0}\n\n'
        b"event: missed\r\n"
        b'data: {"missed": 2}\r\n\r\n'
    )

    def handler(request: httpx.Request) -> httpx.Response:
        assert request.headers["Accept"] == "text/event-stream"
        return httpx.Response(200, content=body)

    session = SECRET.issue("com.hashinbox")
    async with client(handler, session=session) as api:
        raw = [event async for event in logs_stream(api, LogsStreamRequest())]
        typed = [event async for event in api.stream(LogsStreamRequest())]
    assert [event.event for event in raw] == ["message", "missed"]
    assert [event.data for event in typed] == [{"id": 0}, {"missed": 2}]


CLOSE = b"event: close\ndata: {}\nretry: 0\n\n"


def entries(*ids: int) -> bytes:
    return b"".join(f'data: {{"n": {n}}}\nid: {n}\n\n'.encode() for n in ids)


async def test_a_stream_reconnects_each_window_after_its_last_event() -> None:
    answers = [entries(1, 2) + CLOSE, entries(3) + CLOSE, entries(4)]
    sent: list[str | None] = []

    def handler(request: httpx.Request) -> httpx.Response:
        assert "Last-Event-ID" not in request.headers
        sent.append(request.url.params.get("cursor"))
        return httpx.Response(200, content=answers.pop(0))

    async with client(handler) as api:
        handle = logs_stream(api, LogsStreamRequest(cursor="0"))
        read = [json.loads(event.data)["n"] async for event in handle]
    assert read == [1, 2, 3, 4]
    assert sent == ["0", "2", "3"], "the caller's own cursor first"
    assert handle.cursor == "4"


async def test_stop_ends_a_stream_for_good() -> None:
    calls = 0

    def handler(request: httpx.Request) -> httpx.Response:
        nonlocal calls
        calls += 1
        return httpx.Response(200, content=entries(calls) + CLOSE)

    async with client(handler) as api:
        read = []
        async with api.stream(LogsStreamRequest()) as handle:
            async for event in handle:
                read.append(event.data["n"])
                if len(read) == 2:
                    await handle.stop()
    assert read == [1, 2]
    assert calls == 2


async def test_a_failed_connect_backs_off_and_a_refused_one_raises() -> None:
    answers = [
        httpx.Response(503, json={"kind": "unavailable", "message": "down"}),
        httpx.Response(503, json={"kind": "unavailable", "message": "down"}),
        httpx.Response(200, content=entries(1) + CLOSE),
        httpx.Response(401, json={"kind": "unauthenticated", "message": "no"}),
    ]
    at: list[float] = []

    def handler(request: httpx.Request) -> httpx.Response:
        at.append(time.monotonic())
        return answers.pop(0)

    read = []
    async with client(handler, backoff=0.05, max_backoff=0.08) as api:
        with pytest.raises(ApiError) as refused:
            async for event in logs_stream(api, LogsStreamRequest()):
                read.append(event.id)
    assert read == ["1"]
    assert refused.value.status == 401
    waits = [later - earlier for earlier, later in itertools.pairwise(at)]
    assert waits[0] >= 0.05 and waits[1] >= 0.08, waits
    assert waits[2] < 0.05, "a window's close reconnects at once"


def test_any_error_is_an_api_error() -> None:
    refused = ApiError(
        "no", kind=Kind.UNAUTHENTICATED, reason="totp_required", status=401
    )
    assert ApiError.of(refused) is refused
    assert ApiError.of(refused).reason == Reason.TOTP_REQUIRED

    down = httpx.ConnectError("refused")
    error = ApiError.of(down)
    assert (error.status, error.kind, error.message) == (0, Kind.UNAVAILABLE, "refused")
    assert error.__cause__ is down
    assert ApiError.of(TimeoutError()).kind == Kind.UNAVAILABLE

    plain = ValueError("bad")
    error = ApiError.of(plain)
    assert (error.status, error.kind, error.reason) == (0, Kind.UNAVAILABLE, None)
    assert (error.message, error.__cause__) == ("bad", plain)

    odd = ApiError.of("odd")
    assert (odd.status, odd.kind, odd.reason, odd.message) == (
        0,
        Kind.UNAVAILABLE,
        None,
        "odd",
    )

    made = ApiError("made")
    assert (made.status, made.kind, made.reason, made.metadata) == (
        0,
        Kind.UNAVAILABLE,
        None,
        {},
    )


def test_an_answer_not_the_apis_error_is_unavailable() -> None:
    html = "<html>" + "x" * 2000 + "</html>"
    other = '{"status": "down"}'
    strange = '{"kind": "strange", "message": "no"}'
    for status, text in [(502, html), (503, ""), (503, other), (500, strange)]:
        error = ApiError.of(httpx.Response(status, text=text))
        assert (error.status, error.kind, error.reason, error.message) == (
            status,
            Kind.UNAVAILABLE,
            None,
            f"could not parse the response (HTTP {status})",
        )
        assert error.metadata == {"response": text[:1024]}
    body = {
        "kind": "permission_denied",
        "message": "no",
        "reason": "missing_scope",
        "metadata": {"scope": "s"},
    }
    error = ApiError.of(httpx.Response(403, json=body))
    assert (error.status, error.kind, error.message) == (
        403,
        Kind.PERMISSION_DENIED,
        "no",
    )
    assert (error.reason, error.metadata) == (Reason.MISSING_SCOPE, {"scope": "s"})

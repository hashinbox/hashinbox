"""Against a running server: `HASHINBOX_URL`, and `HASHINBOX_KEY_ID` with
`HASHINBOX_KEY_SECRET` for the calls a key makes."""

import os

import pytest

from hashinbox import Client, Secret, SessionGetRequest, health_get

URL = os.environ.get("HASHINBOX_URL")

pytestmark = pytest.mark.skipif(URL is None, reason="HASHINBOX_URL is not set")


async def test_health() -> None:
    assert URL is not None
    async with Client(URL) as api:
        await health_get(api)


async def test_a_key_session() -> None:
    assert URL is not None
    id, value = (
        os.environ.get("HASHINBOX_KEY_ID"),
        os.environ.get("HASHINBOX_KEY_SECRET"),
    )
    if id is None or value is None:
        pytest.skip("HASHINBOX_KEY_ID and HASHINBOX_KEY_SECRET are not set")
    async with Client(URL, secret=Secret.parse(id, value)) as api:
        # Raw: the generated `SessionRead.kind` decodes as the error `Kind`.
        read = await api.send(SessionGetRequest())
        assert read["key"]["id"] == id
        assert api.session is not None and api.session.issuer == id

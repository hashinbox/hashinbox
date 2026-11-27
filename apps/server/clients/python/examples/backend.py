"""A backend holding a key: it creates a wallet for a customer, then issues
that customer's browser a 15-minute token reading only that wallet. The
secret stays here; the client signs its own sessions with it.

    HASHINBOX_URL=… HASHINBOX_KEY_ID=hip/… HASHINBOX_KEY_SECRET=his/… \
        python examples/backend.py
"""

import asyncio
import os

from hashinbox import (
    Client,
    CreateWalletRequest,
    Secret,
    WalletsCreateRequest,
    escape_id,
    wallets_create,
)


async def main() -> None:
    secret = Secret.parse(
        os.environ["HASHINBOX_KEY_ID"], os.environ["HASHINBOX_KEY_SECRET"]
    )
    async with Client(os.environ["HASHINBOX_URL"], secret=secret) as api:
        body = CreateWalletRequest(
            chain="ethereum", network="mainnet", external_id="customer-1"
        )
        wallet = await wallets_create(api, WalletsCreateRequest(body=body))
    scope = f"com.hashinbox.wallets[{escape_id(wallet.id)}]:read"
    token = secret.issue(scope, subject="customer-1", ttl=15 * 60)
    print(wallet.id)
    print(token.token)


if __name__ == "__main__":
    asyncio.run(main())

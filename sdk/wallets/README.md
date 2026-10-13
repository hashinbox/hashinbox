# wallets

Create wallets, import keys, and build, sign and broadcast transactions on any
chain without naming a chain crate. `Factories` holds one wallet factory and broadcaster per network;
a chain's `Wallet` is one address with its key,
transacting any asset of its network; a `WalletsStore` files its address and
key and restores it by address. Labels, archiving and watch-only addresses are
the application's. Handled failures are `Error` inside `anyhow::Error`. Its core depends on `base` and `crypto`;
`payments::config::Composition` assembles configured chains into one index and
one factory map.

## Usage

```rust
use std::sync::Arc;

use base::{Asset, Broadcaster, Network};
use wallets::{Factories, FactoryBroadcaster, SecretBytes, Factory};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `factory` and `broadcaster` come from a chain crate. Nothing below names a chain.
async fn run(
    factory: impl Factory + 'static,
    broadcaster: Arc<dyn Broadcaster>,
    network: Network,
    coin: Asset,
    key: SecretBytes,
) -> Result<()> {
    let mut factories = Factories::new();
    factories.set(FactoryBroadcaster {
        network: network.clone(),
        factory: Arc::new(factory),
        broadcaster,
    });

    // Factories make wallets; a `WalletsStore` would file them.
    let treasury = factories.from_pk(&network, key).await?;
    let (alice, _alice_key) = factories.generate(&network).await?;
    let to = factories.parse(&network, &alice.address().value)?;

    // Build and sign with the wallet's own key, then broadcast its exact bytes
    // through the network's broadcaster.
    let mut transaction = treasury.transaction(&coin)?;
    transaction.transfer(to, "0.5".parse()?)?;
    let signed = transaction.sign().await?;
    let broadcaster = factories.get(&network)?.broadcaster.clone();
    for transaction in &signed {
        broadcaster.broadcast(transaction).await?;
    }
    Ok(())
}
```

## Notes

- `Factories` persists no wallet or key. Load a wallet when needed with
  `WalletsStore::get_by_address`, which rebuilds its stored key through
  `Factories::from_pk`. A store files a `Wallet`'s address and key (from
  `Wallet::private_key`); keyless watched addresses are the application's
  rows, not a `Wallet`. Register a new
  wallet's address with the index (`watch`) before you store its key: a crash
  in between then leaves an address that is watched without a key, never a key
  whose deposits nobody observes.
- Sending is not here: the application queues it (the server's send job saves
  the signed bytes, then broadcasts them; indexing records them once mined). A
  transaction builder fetches its own chain inputs; the broadcaster sends the
  exact bytes to every node.
- An accepted broadcast means submitted, not confirmed. Confirmation comes from
  the index.

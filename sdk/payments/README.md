# payments

The container: every configured network built over the application's
stores. Per network it keeps the `base::Service` its chain's `base::ServiceFactory`
built, and the `rpc::Swap` every part of that service reaches its nodes
through. It names no chain; every call is keyed by network.

- `Payments::builder()`: `with_chain::<C>(chain)` registers the chain's
  `base::ServiceFactory`, built from a `base::NetworkConfig` (`C: TryFrom<&NetworkConfig>`);
  `with_network_config(config)` adds a network its chain builds;
  `with_network(config, endpoints)` adds a chain's own config over its
  nodes; `with_client(network, client)` reaches that network through
  `client` instead; `with_nodes(network, nodes)` reaches it through
  `Nodes` instead of its configured endpoints; `with_storage(base::Storage)`
  (required). `build()`
  clusters each network's nodes (`ServiceFactory::info`, primary by its `tip()`;
  `rpc::Error::is_retryable` errors retried), wraps that in a `Swap` and builds the service
  over it, asking no node; a network that does not build is left out and
  `service(network)` says why.
- `Nodes::new(endpoints)`: a network's nodes, clustered by tip;
  `.route("getBlock", [0, 1])?` spreads one method round robin over some of
  them (`rpc::Router`); without routes there is no router.
- `Payments`:
  - services: `service(network)`, `services()`, `networks()`, `assets()`
    (merged), `client(network)` (the `Swap`), `connect(network, &nodes)`
    (moves every part to other nodes, asking none),
    `info(network, endpoint)` (asks one node, as a one-node cluster, its chain
    and tip);
  - indexing: each service's `indexer` (the caller steps `next()`, about
    every `interval()`), `watch(address, asset, birthday)` on the address's
    network, `stats()`;
  - wallets: `generate(network)` (kept in the wallet store),
    `wallet(address)`, `wallets()` (restored through the factories),
    `parse(network, text)`, `broadcast(network, signed)`.

```rust
use std::sync::Arc;

use base::{Chain, NetworkConfig, ServiceFactory, Storage};
use payments::Payments;

/// `C` builds `network`'s chain's configuration.
async fn serve<C>(
    network: NetworkConfig,
    storage: Storage,
    other_nodes: Arc<dyn rpc::Client>,
) -> anyhow::Result<()>
where
    C: ServiceFactory + for<'a> TryFrom<&'a NetworkConfig, Error = anyhow::Error> + 'static,
{
    let payments = Payments::builder()
        .with_chain::<C>(Chain::from_static("bitcoin"))
        .with_network_config(network)
        .with_storage(storage)
        .build()
        .await?;
    for (network, stats) in payments.stats() {
        println!("{}: {stats:?}", network.name);
    }
    if let Some(network) = payments.networks().next() {
        let (wallet, _secret) = payments.generate(network).await?;
        println!("new wallet {}", wallet.address().value);
        // Every part of the network moves to the other nodes at once.
        payments.client(network)?.replace(other_nodes, Vec::new());
    }
    Ok(())
}
```

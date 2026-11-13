# base

The chain-neutral vocabulary every other SDK crate shares: networks, addresses,
assets, exact amounts, blocks and transaction identities. It also declares the
small traits a chain implements to build and broadcast a transaction.

## Usage

```rust
use base::{Address, Asset, Chain, Decimal, Network};

const CHAIN: Chain = Chain::from_static("example");

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let network = Network::new(CHAIN, "testnet");
let address = Address::new(network.clone(), "addr1");
assert!(address.belongs_to(&network));
assert!(Asset::native(network.clone()).is_native());

// Amounts are exact decimals, converted to atomic units without rounding.
let amount: Decimal = "0.015".parse()?;
assert_eq!(amount.to_atomic_u64(8)?, 1_500_000);

# Ok(())
# }
```

## Notes

- `Container<T: Transport>` (`base::container`) keeps every registered
  network's `Service` (or why it did not build) and its transport.
  `register::<F>(config, transport)` reads the chain's settings
  (`ServiceFactory::new(&config)`) and builds its service over the
  container's `Storage` and `transport.client()`, asking no node. `service`,
  `transport` and the cross-network calls (`networks`, `assets`, `stats`,
  `watch`, `generate`, `wallet`, `wallets`, `parse`, `broadcast`) are keyed
  by network. A `Transport` is how a network gets its one `rpc::Client`:
  `Simple` is a client made upfront (tests, custom stacks); the server's
  selects among its stored nodes.
- `InfoClient` is a chain's reader of one node: `new(node)`, `tip()`, and
  `info()`, its chain and tip as the node reports them (the caller judges
  them). `CachedInfo` asks its tip at most once per `ttl`, the network's
  block time (`base::Cache`), and `last()` answers without asking.
- `Address::new` stores the text it is given and does not validate or normalize
  it. Addresses are compared as strings, so parse user input with the chain's
  wallet factory; a differently spelled address is a different address and its
  history will not be found.

`Assets` (one `AssetInfo` per asset) resolves display precision by `Asset`, which is network-scoped: the same
token address on different networks is two assets and may declare different decimals.

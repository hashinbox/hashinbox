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

- `Cluster` makes several nodes of one network one `rpc::Client`. Each node
  is the chain's info client over it (an `InfoClient`: `tip()`, `info()`);
  `ServiceFactory::info(nodes)` is the chain's cluster of them. Requests go to the
  primary, the highest `tip()` as `Policy` selects (refresh, lag, failure
  streak, rate limit); a broadcast reaches every node once. It is an
  `InfoClient` too, so clusters nest.
- `Address::new` stores the text it is given and does not validate or normalize
  it. Addresses are compared as strings, so parse user input with the chain's
  wallet factory; a differently spelled address is a different address and its
  history will not be found.

`Assets` (one `AssetInfo` per asset) resolves display precision by `Asset`, which is network-scoped: the same
token address on different networks is two assets and may declare different decimals.

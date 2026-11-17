# chain-bitcoin

Bitcoin for the payment SDK: reads blocks as `base` blocks with every leg,
and builds, signs and broadcasts BTC transfers from P2WPKH and P2TR wallets.
The `native` module is the protocol code alone and can be used without the
rest of the SDK.

## Usage

```rust,no_run
use std::sync::Arc;

use base::{
    Asset, Broadcaster as _, Chained as _, ServiceFactory as _, Storage, Transfer,
    WalletFactory as _,
};
use chain_bitcoin::{AddressType, Client, Config, Factory, Network};
use rpc::Endpoint;

/// `storage` is the application's stores; its `inputs` are the unspent
/// outputs coin selection spends.
async fn bitcoin(storage: Storage) -> anyhow::Result<()> {
    let mut config = Config::new(Network::Regtest);
    config.address_type = AddressType::Taproot;
    let node: Arc<dyn rpc::Client> =
        Arc::new(rpc::node(&Endpoint::new("http://bitcoin:bitcoin@127.0.0.1:18443"), Default::default())?);
    let rpc = Client::new(node.clone(), config.network);
    let factory = Factory::from(config);

    // What `base::ServiceFactory::service` builds a stored network into:
    // indexer, wallet factory, broadcaster and assets over the application's stores.
    let _service = factory.service(&storage, node)?;

    // Reading: what `base::Indexer` consumes.
    let chain = factory.chain(&rpc)?;
    let tips = chain.tips().await?;
    let _blocks = chain.blocks(tips.confirmed, tips.produced).await?;

    // Sending: one transfer signed into one transaction; keep the bytes,
    // then broadcast them to every node.
    let wallets = factory.wallets(&rpc, Arc::clone(&storage.inputs));
    let wallet = wallets.from_pk(wallets.generate().await?).await?;
    let builder = wallet.transaction(&Asset::native(Network::Regtest.into()))?;
    let to = wallets.parse_address("bcrt1q...")?;
    let transaction = builder.sign(Transfer::new(to, 10_000_u64.into())).await?;
    rpc.broadcast(&transaction).await?;

    Ok(())
}
```

`Config::try_from(&base::NetworkConfig)` builds it from a stored network: the
network by name, BTC's display decimals, `confirmation_depth`
(`confirmations + 1`), `max_depth`, and from `settings` the knobs
`address_type`, `fee_target_blocks`, `fee_floor`, `fee_ttl_ms`,
`interval_ms`, and `batch`, the blocks the indexer fetches per step
(default 100) (each absent one its default).

## Notes

- `Config` is data; `Factory` builds from it. As the chain's
  `base::ServiceFactory`, `Factory::new(&network_config)` reads a stored
  network as its `Config`; `Factory::from(config)` takes one made in code.
  `factory.chain(&rpc)` is the block reader, `factory.wallets(..)` the
  wallet factory, and `factory.service(&storage, client)` validates and
  returns the whole `base::Service`, every part over the given client,
  asking no node. The indexer wakes every `interval_ms` milliseconds
  (default 30,000).
- The config holds no endpoints. `Info` is the chain's `base::InfoClient`
  over one node (`Info::new(node)`): `tip()` is `getblockcount`, `info()`
  the network it serves, by name and its tip, as the node reports them; the caller judges them.
  `Client` extends `Info` with every other call.
- Position and height are both the block height. `confirmation_depth`
  (1 to 1000, default 6) makes a block final once that many blocks, itself
  included, are on the chain: `confirmed = produced - (depth - 1)`. Bitcoin
  has no finality of its own.
- `blocks(start, end)` makes two JSON-RPC batches: every `getblockhash`, then
  every `getblock <hash> 3`. Verbosity 3 carries each input's `prevout`, so
  the node must be Bitcoin Core 23 or later and unpruned; no `-txindex` is
  needed. Keep endpoint response bounds large enough for a batch of blocks.
- Legs: `Received` per output in output order, then per input `Spent` (with
  the coin it consumes) and `Fee` for its leftover. Inputs fill outputs in
  order, so the fee is normally the last input's leftover. The coinbase has
  `Received` legs only: its outputs are coins like any other.
- Scripts without an address (`OP_RETURN`, bare multisig, other
  non-standard scripts) get no leg.
- Fees come from `estimatesmartfee` (conservative, `fee_target_blocks`),
  never below `fee_floor`; the floor alone prices a send when the node cannot
  estimate, such as on a fresh regtest chain. The factory's one fee
  estimator (`WalletFactory::fees`), which every builder prices with too,
  is two: a precise one (`fee.rs`) that asks the node for the rate every
  time, and a cached one (`fee_cache.rs`) around it that keeps the rate for
  `fee_ttl_ms` (default 60 s); a send's size comes from the store's coins,
  not the node, so only the rate is kept. Its estimate is one
  transaction spending every coin of every source to one 34-byte output, no
  change: change is made only when it is above dust after paying for itself;
  a smaller leftover is fee.
- A send pays one transfer in one transaction: one payment output, spending
  the store's unspent outputs of the wallet, largest first, with change back
  to the wallet. Nothing reserves coins: a second send
  before the first is mined can pick the same coins and be refused. A stored
  output does not say whether it is a coinbase, so an immature coinbase coin
  can be picked and the node refuses the send.
- Broadcasting sends the exact signed bytes to every node; a node that
  already holds the transaction counts as accepting it. Acceptance means
  submitted, not confirmed.
- The chain has no `Confirmer`: a sent transaction's index (`base`'s
  `TransactionIndex`) left behind the tip less `max_depth` is given up, as a
  wallet index jumps. Its line is `Line::Depth(max_depth)` (none: no line).

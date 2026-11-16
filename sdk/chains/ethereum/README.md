# chain-ethereum

Ethereum for the payment SDK: reads blocks as `base` blocks with every leg of
ether and the allowlisted ERC-20 tokens, and builds, signs and broadcasts ETH
and token transfers as EIP-1559 transactions.

## Usage

```rust,no_run
use base::{
    Asset, Broadcaster as _, BuilderExt as _, Chained as _, ServiceFactory as _, Storage,
    WalletFactory as _,
};
use std::sync::Arc;

use chain_ethereum::{Client, Config, Factory, Network};
use rpc::Endpoint;

async fn ethereum(storage: Storage) -> anyhow::Result<()> {
    let config = Config::new(Network::new(1, "mainnet"));
    let node: Arc<dyn rpc::Client> = Arc::new(rpc::node(&Endpoint::new("https://node.invalid/v3/KEY"), Default::default())?);
    let rpc = Client::new(node.clone(), config.native());
    let ether = Asset::native((&config.native()).into());
    let factory = Factory::from(config);

    // What `base::ServiceFactory::service` builds a stored network into:
    // indexer, wallet factory, broadcaster and assets over the application's stores.
    let _service = factory.service(&storage, node)?;

    // Reading: what `base::Indexer` consumes.
    let chain = factory.chain(&rpc);
    let tips = chain.tips().await?;
    let _blocks = chain.blocks(tips.confirmed, tips.produced).await?;

    // Sending: sign, keep the bytes, then broadcast them to every node. Fees
    // are priced for a new recipient unless `Holdings` say it holds the asset.
    let wallets = factory.wallets(&rpc, Arc::new(base::NoHoldings));
    let wallet = wallets.from_pk(wallets.generate().await?).await?;
    let mut builder = wallet.transaction(&ether)?;
    builder.send((wallets.parse_address("0x...")?, 10_000_u64.into()))?;
    for transaction in builder.sign().await? {
        rpc.broadcast(&transaction).await?;
    }

    Ok(())
}
```

`Config::try_from(&base::NetworkConfig)` builds it from a stored network:
`chain_id`, assets (ether's display decimals, the `contract`
tokens), `confirmations`, and from `settings` the knobs
`limits`, `fee_ttl_ms`, `pending_ttl_ms`, `interval_ms`, and `batch`, the
blocks the indexer fetches per step (default 100; lower it for a node that
rate-limits) (each absent one its default). It has no `max_depth`: it reads
balances (`base::Balances`, at the node's `finalized` block), so its
indexer reads nothing at or below that block and settles an index behind it
with one balance read (design 083 §9).

## Notes

- `Config` is data; `Factory` builds from it. As the chain's
  `base::ServiceFactory`, `Factory::new(&network_config)` reads a stored
  network as its `Config`; `Factory::from(config)` takes one made in code.
  `factory.chain(&rpc)` is the block reader, `factory.wallets(..)` the
  wallet factory, and `factory.service(&storage, client)` validates and
  returns the whole `base::Service`, every part over the given client,
  asking no node. The indexer wakes every `interval_ms` milliseconds
  (default 12,000).
- The config holds no endpoints. `Info` is the chain's `base::InfoClient`
  over one node (`Info::new(node)`): `tip()` is `eth_blockNumber`, `info()`
  its chain ID and its tip, as the node reports them; the caller judges them.
  `Client` extends `Info` with every other call.
- Position and height are the block number. A block is confirmed once it is at
  or below the node's `finalized` block; a node that serves none yet (syncing,
  a development chain) fails `tips`.
- `blocks` is two JSON-RPC batches for the whole range: the blocks, then each
  block's `eth_getBlockReceipts` (or, on a node without it, one
  `eth_getTransactionReceipt` per transaction). Only standard `eth_*`
  methods are used.
- Legs: ether `value` and allowlisted ERC-20 `Transfer` logs are a `Spent` and
  a `Received` leg (a token mint is `Minted`, a burn `Burned`); the fee
  (`gasUsed * effectiveGasPrice`) is a `Fee` leg of the sender. A reverted
  transaction keeps only its fee. Withdrawals are rewards.
- Ether a contract moves inside a transaction (internal transfers, such as a
  swap paying out ether) appears in no transaction field or log, so it is not
  indexed: a wallet paid that way shows too low a balance. Only tracing
  (`debug_*`, `trace_*`, not standard RPC) could see it; this crate does not
  use it. ERC-20 tokens are unaffected: every transfer emits a log.
- A builder sends one asset: ether, or one configured token (`transfer(to,
  amount)` calldata to the contract, no ether); any other asset is
  `Unsupported`. A batch is one transaction per transfer, with the node's
  pending nonce and the next ones, all signed before anything is broadcast.
  Gas and fees come from the factory's one fee estimator
  (`WalletFactory::fees`), which every builder signs with too, so an
  estimate and the transaction signed after it agree. It is two: a precise
  one (`fee.rs`) that asks the node every time, exactly for the transfer
  given (`eth_estimateGas` from its sender to its recipient, or to a fresh
  unused address without one, plus the margin; the price from the node's tip
  and the latest base fee), and a cached one (`fee_cache.rs`) around it. On
  Ethereum the price is the same for every send, and a transfer's gas
  depends only on its asset and on whether its recipient holds it already,
  not on who sends it: so the cached one keeps the price, and the gas of
  each (asset, recipient holds it, as `Holdings` say), for `fee_ttl_ms`
  (default 12 s), asking the node once per kind of send however many
  wallets send. A sender holding none of a token cannot be tested (the node
  runs the transfer) and gets its kind's last answer. A signed transaction's
  `Fee` leg is its worst case, `gas limit * max fee`, which is what an
  estimate sums.
- A broadcast refused as "already known" or "nonce too low" is acceptance when
  the node holds that exact hash.

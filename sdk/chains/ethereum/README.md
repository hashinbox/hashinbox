# chain-ethereum

Ethereum for the payment SDK: reads blocks as `base` blocks with every leg of
ether and the allowlisted ERC-20 tokens, and builds, signs and broadcasts ETH
and token transfers as EIP-1559 transactions.

## Usage

```rust,no_run
use base::{
    Asset, Broadcaster as _, Chained as _, ServiceFactory as _, Storage, Transfer,
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
    let _blocks = chain.blocks(tips.confirmed(chain.confirmations()), tips.position).await?;

    // Sending: one transfer signed into one transaction; keep the bytes,
    // then broadcast them to every node. Fees are priced for a new recipient
    // unless `Holdings` say it holds the asset; nonces follow the sends in
    // flight a `TransactionIndexStore` holds.
    let wallets = factory.wallets(&rpc, Arc::new(base::NoHoldings), Arc::new(base::NoTransactionIndexes));
    let wallet = wallets.from_pk(wallets.generate().await?).await?;
    let mut builder = wallet.transaction(&ether)?;
    let to = wallets.parse_address("0x...")?;
    builder.from_transfer(Transfer::new(to, 10_000_u64.into()))?;
    let transaction = builder.sign().await?;
    rpc.broadcast(&transaction).await?;

    Ok(())
}
```

`Config::try_from(&base::NetworkConfig)` builds it from a stored network:
`chain_id`, assets (ether's display decimals, the `contract`
tokens), `confirmations`, and from `settings` the knobs
`limits`, `fee_ttl_ms`, `interval_ms`, `batch`, the
blocks the indexer fetches per step (default 100; lower it for a node that
rate-limits), and the `base::Tracking` of sent payments: `check_ms`
(default 30 s), `bump_after` (50 blocks) and `max_bumps` (5) (each absent
one its default). It has no `max_depth`: it reads
balances (`base::Balances`, at the confirmed block, `confirmations` below the
tip, default 64: about two epochs, where `finalized` sits), so its
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
- Position and height are the block number. `tips` is `eth_blockNumber`; a
  block is confirmed once `confirmations` below it (`Tips::confirmed`).
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
  `Unsupported`. It signs one transfer into one transaction, at the
  sender's next nonce: the node's pending count, or one above the highest
  nonce of the sender's sends in flight
  (`TransactionIndexStore::find_sent`) when
  that is higher, so a node behind a load balancer that has not seen the
  last send yet does not hand its nonce out again.
  The rate (`base::FeeRate`) is the latest base fee and the node's tip; a
  builder signs its tip and a max fee of `2 × base fee + tip`, kept
  `fee_ttl_ms` (default 12 s) in one
  `base::FeeRateProviderCached` shared by every builder, the estimator and
  `Service::rates`. A builder signing a new payment asks the node for its
  gas: `eth_estimateGas` of that very payment (sender, recipient, value or
  calldata), plus the margin (`FeeLimits::gas_limit`); nothing kept is
  reused. A payment the node says would revert fails `Reason::Reverted`,
  one the sender cannot fund `Reason::InsufficientFunds`; nothing is
  signed. The factory's fee estimator (`WalletFactory::fees`, `Estimator`,
  `fee_estimator.rs`) only guesses, for triggers: it asks the node for the
  transfer given (to its recipient, or to a fresh unused address without
  one, plus the margin) and keeps the answer `fee_ttl_ms` per (asset,
  recipient holds it, as `Holdings` say), once per kind of send however
  many wallets send. A sender holding none of a token cannot be tested
  (the node runs the transfer) and gets its kind's last answer. A signed
  transaction's `Fee` leg is its worst case, `gas limit * max fee`, which
  is what an estimate sums.
- `Fee::Set(level)` sets the tip at half, the same or twice the node's
  (`Low`, `Normal`, `High`); the max fee follows it. A builder's saved
  state (`save`) is its payment, source, nonce, gas limit and rate; after
  `load`, `sign` keeps the nonce and gas limit and both fees at least 10%
  (rounded up) over the saved ones: the replacement a node accepts.
  `Fee::Times(1.1)` is that over the higher of today's rate and the saved
  one. Above `FeeLimits` (each fee per gas, the worst-case total), `sign`
  fails `FeeCap`.
- A broadcast refused as "already known" or "nonce too low" is acceptance when
  the node holds that exact hash.
- Its line is the block after the confirmed one (`Line::Final`). A sent
  transaction's index left behind it is settled by the `Confirmer`: its
  receipt (`eth_getTransactionReceipt`) at or below the confirmed block
  is where it is found; none, it is not mined through that block.

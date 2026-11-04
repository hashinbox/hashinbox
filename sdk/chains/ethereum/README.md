# chain-ethereum

Ethereum for the payment SDK: reads blocks as `base` blocks with every leg of
ether and the allowlisted ERC-20 tokens, and builds, signs and broadcasts ETH
and token transfers as EIP-1559 transactions.

## Usage

```rust,no_run
use base::{Asset, Broadcaster as _, BuilderExt as _, Chained as _, Config as _, Storage, WalletFactory as _};
use chain_ethereum::{Config, Network};
use rpc::Endpoint;

async fn ethereum(storage: Storage) -> anyhow::Result<()> {
    let config = Config::new(
        Network::new(1, "mainnet"),
        vec![Endpoint::new("https://node.invalid/v3/KEY")],
    );
    // The one entry point a container uses: indexer, factory, broadcaster,
    // and assets over the application's stores.
    let _service = config.build(&storage, config.client().await?).await?;

    // Checks the chain ID and every token's decimals.
    let rpc = config.connect().await?;

    // Reading: what `base::Indexer` consumes.
    let chain = config.chain(&rpc);
    let tips = chain.tips().await?;
    let _blocks = chain.blocks(tips.confirmed, tips.produced).await?;

    // Sending: sign, keep the bytes, then broadcast them to every node.
    let factory = config.factory(&rpc);
    let wallet = factory.from_pk(factory.generate().await?).await?;
    let ether = Asset::native((&config.native()).into());
    let mut builder = wallet.transaction(&ether)?;
    builder.send((factory.parse_address("0x...")?, 10_000_u64.into()))?;
    for transaction in builder.sign().await? {
        rpc.broadcast(&transaction).await?;
    }

    Ok(())
}
```

`Config::try_from(&base::NetworkConfig)` builds it from a stored network:
`chain_id`, endpoints, assets (ether's display decimals, the `contract`
tokens), `confirmations`, `max_history`, and from `settings` the knobs
`limits`, `fee_ttl_ms`, `pending_ttl_ms`, `interval_ms` (each absent one its
default).

## Notes

- `Config` implements `base::Config`: `build(&storage, client)` validates,
  verifies the node and returns the `base::Service`. The indexer wakes every
  `interval_ms` milliseconds (default 12,000).
- `client()` is the chain's own cluster over `endpoints`; `build(&storage,
  client)` verifies once that `client` serves the network and builds every
  part over it. Pass a `rpc::Swap` and keep it to change nodes live.
  Endpoints are needed only by `client()`.
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
  (`WalletFactory::fees`): EIP-1559 fees cached for `fee_ttl_ms` (default
  12 s); 21 000 gas for ether (an EOA recipient); for a token, one
  `eth_estimateGas` of `transfer(0xfefe…fe, 1)` from its first sender, plus
  the margin, kept for the factory's life. A signed transaction's `Fee` leg
  is its worst case, `gas limit * max fee`, which is what an estimate sums.
- A broadcast refused as "already known" or "nonce too low" is acceptance when
  the node holds that exact hash.

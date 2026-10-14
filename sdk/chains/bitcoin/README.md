# chain-bitcoin

Bitcoin for the payment SDK: reads blocks as `base` blocks with every leg,
and builds, signs and broadcasts BTC transfers from P2WPKH and P2TR wallets.
The `native` module is the protocol code alone and can be used without the
rest of the SDK.

## Usage

```rust,no_run
use std::sync::Arc;

use base::{Asset, Broadcaster as _, Chained as _, InputStore, WalletFactory as _};
use chain_bitcoin::{AddressType, Config, Network};
use json_rpc::Endpoint;

/// `inputs` is the store of unspent outputs, which coin selection spends.
async fn bitcoin(inputs: Arc<dyn InputStore>) -> anyhow::Result<()> {
    let config = Config {
        address_type: AddressType::Taproot,
        ..Config::new(
            Network::Regtest,
            vec![Endpoint::new("http://bitcoin:bitcoin@127.0.0.1:18443")],
        )
    };
    // Checks the node and its network.
    let rpc = config.connect().await?;

    // Reading: what `base::Indexer` consumes.
    let chain = config.chain(&rpc)?;
    let tips = chain.tips().await?;
    let _blocks = chain.blocks(tips.confirmed, tips.produced).await?;

    // Sending: sign, keep the bytes, then broadcast them to every node.
    let factory = config.factory(&rpc, inputs);
    let wallet = factory.from_pk(factory.generate().await?).await?;
    let mut builder = wallet.transaction(&Asset::native(Network::Regtest.into()))?;
    builder.transfer(factory.parse_address("bcrt1q...")?, 10_000_u64.into())?;
    for transaction in builder.sign().await? {
        rpc.broadcast(&transaction).await?;
    }
    Ok(())
}
```

## Notes

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
  estimate, such as on a fresh regtest chain.
- A send spends the store's unspent outputs of the wallet, largest first,
  with change back to the wallet. Nothing reserves coins: a second send
  before the first is mined can pick the same coins and be refused. A stored
  output does not say whether it is a coinbase, so an immature coinbase coin
  can be picked and the node refuses the send.
- Broadcasting sends the exact signed bytes to every node; a node that
  already holds the transaction counts as accepting it. Acceptance means
  submitted, not confirmed.

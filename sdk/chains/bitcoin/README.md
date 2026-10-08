# chain-bitcoin

Bitcoin for the payment SDK: it indexes blocks into history and spendable
outputs, and builds, signs and submits BTC transfers from P2WPKH and P2TR
wallets. The `native` module is the protocol code alone and can be used without
the rest of the SDK.

## Usage

```rust,no_run
use std::sync::Arc;

use chain_bitcoin::{AddressType, Config, Error, Network, Store};
use payments::config::{ComposedChain, Rpc};
use wallets::SubmissionRegistrar;

/// `repository` is the one store every network is indexed into.
async fn bitcoin<R: Store>(
    repository: R,
    registrar: &Arc<dyn SubmissionRegistrar>,
) -> Result<ComposedChain<&'static str>, Error> {
    let rpc = Rpc::endpoint("http://bitcoin:bitcoin@127.0.0.1:18443");

    let config = Config {
        confirmation_depth: 6,
        address_type: AddressType::Taproot,
        ..Config::new(Network::Regtest, rpc, "btc")
    };

    // Checks the node and its network. Opens and writes nothing.
    let chain = config.connect().await?;
    // The index and the wallet factory for the "btc" tag.
    chain.compose(repository, registrar).await
}
```

An application that reads a configuration file registers the chain instead:
`registry.register::<chain_bitcoin::Config<Tag>>("bitcoin")`.

## Notes

- `confirmation_depth` is the one safety number you choose, from 1 to 1000,
  default 6. Bitcoin has no finality, so blocks at that depth are stored as
  final: a reorg deeper than it quarantines the network, and only an offline
  rescan of that network brings it back.
- The node must be Bitcoin Core 31.x, unpruned, synchronized and started with
  `-txindex`. `connect` refuses anything else, because spent outputs are
  resolved through the transaction index.
- Fees come from the node's `estimatesmartfee` and there is no fixed-rate
  override. A node that has seen too few transactions, such as a fresh regtest
  chain, fails a send with `ErrorKind::FeeUnavailable`.
- Change from a transaction that is not yet in a block cannot be spent. The
  outputs of a submission whose outcome is unknown stay reserved until the index
  shows them spent or the node refuses the transaction, so a second send cannot
  spend them twice. Reservations live in memory and are lost on restart.

A send accepts `wallets::Amount::All` to spend every mature, unreserved indexed
output. Input pagination and reservation stay in the existing outbox. The
sweep pays its residual after fees to its destination with no change; a dust
residual is refused. In batches, the sweep must be its source's final item,
after any fixed payments.

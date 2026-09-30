# chain-ethereum

Ethereum for the payment SDK: it indexes blocks into history and balances, and
builds, signs and submits ETH and ERC-20 transfers as EIP-1559 transactions. The
`native` module is the protocol code alone and can be used without the rest of
the SDK.

## Usage

```rust,no_run
use std::{collections::BTreeMap, sync::Arc};

use chain_ethereum::{Config, Error, Network, Store};
use sdk_config::Rpc;
use wallets::{ComposedChain, SubmissionRegistrar};

/// `repository` is the one store every network is indexed into.
async fn ethereum<R: Store>(
    repository: R,
    registrar: &Arc<dyn SubmissionRegistrar>,
) -> Result<ComposedChain<&'static str>, Error> {
    let secrets = BTreeMap::from([("RPC_KEY".to_owned(), "secret".to_owned())]);
    let rpc = Rpc::endpoint("https://node.invalid/v3/${RPC_KEY}");

    let config = Config::new(Network::new(1, "mainnet"), rpc, "eth");

    // Checks the chain ID, block tracing and every token. Opens and writes nothing.
    let chain = config.connect(&secrets).await?;
    // The index and one wallet factory per asset tag.
    chain.compose(repository, registrar).await
}
```

An application that reads a configuration file registers the chain instead:
`registry.register::<chain_ethereum::Config<Tag>>("ethereum")`. ERC-20 tokens
are listed in that section:

```toml
[chains.ethereum]
network = 'mainnet'
chain_id = 1
rpc.endpoints = ['https://node.invalid/v3/${RPC_KEY}']
tokens = [{ contract = '0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48', asset = 'usdc', decimals = 6 }]
```

## Notes

- The node must serve `debug_traceBlockByHash` with `prestateTracer` in
  `diffMode`, and `connect` refuses one that does not. Ether moved by a contract
  appears in no transaction field and no log, so without the trace a wallet paid
  through a contract would have a wrong balance.
- Finality is the node's own `finalized` block, so there is no confirmation
  depth to configure and the key is rejected. Confirmed history appears only
  after a block is finalized, and a node that reports no finalized block indexes
  nothing.
- A batch takes consecutive nonces per sender and is checked and signed in full
  before the first broadcast. If a broadcast fails midway, the transactions
  before it are already accepted and the ones after it are not sent.
- ETH and ERC-20 wallets of one address must share one send pipeline, or they
  reuse a nonce. `Config` does this for you; when building factories by hand,
  create the token factory with `WalletFactory::sibling`.
- A transaction signed through `TransferBuilder` but not broadcast reserves
  nothing. A later send may take its nonce, and its broadcast then fails.
- A configuration with tokens accepts exactly one RPC endpoint.

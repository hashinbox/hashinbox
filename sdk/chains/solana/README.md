# chain-solana

Solana for the payment SDK: it indexes finalized blocks into history and
balances for SOL and SPL tokens, and builds, signs and submits native SOL
transfers. The `native` module is the protocol code alone and can be used
without the rest of the SDK.

## Usage

```rust,no_run
use std::{collections::BTreeMap, sync::Arc};

use chain_solana::{Config, Network, Store};
use sdk_config::Rpc;
use wallets::{ComposedChain, SubmissionRegistrar};

/// `repository` is the one store every network is indexed into.
async fn solana<R: Store>(
    repository: R,
    registrar: &Arc<dyn SubmissionRegistrar>,
) -> Result<ComposedChain<&'static str>, Box<dyn std::error::Error>> {
    let secrets = BTreeMap::<String, String>::new();
    let genesis = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d".parse()?;

    let config = Config::new(
        Network::new("mainnet", genesis),
        Rpc::endpoint("http://127.0.0.1:8899"),
        "sol",
    );

    // Checks the genesis hash and every token mint. Opens and writes nothing.
    let chain = config.connect(&secrets).await?;
    // The index and one wallet factory per asset tag.
    Ok(chain.compose(repository, registrar).await?)
}
```

An application that reads a configuration file registers the chain instead:
`registry.register::<chain_solana::Config<Tag>>("solana")`. SPL tokens are
listed in that section:

```toml
[chains.solana]
network = 'mainnet'
genesis_hash = '5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d'
rpc.endpoints = ['http://127.0.0.1:8899']
tokens = [{ mint = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v', asset = 'usdc', decimals = 6 }]
```

## Notes

- Configure exactly one RPC endpoint. Every request is sent once and never
  retried or failed over, because a repeated submission can send the same
  payment twice. Set the endpoint's timeout with that in mind.
- The genesis hash is configured and verified, never discovered. `connect`
  refuses a node that reports another hash, so a wrong URL cannot index a
  different cluster into the same store.
- A signed transaction expires with its blockhash. When a submission's outcome
  is unknown, the sender stays reserved until the node or the index shows the
  transaction, or the blockhash expires unseen; a second send from that sender
  fails with `SourceBusy` instead of waiting.
- SPL tokens are received and reported, not sent. `Wallets::author` and
  `Wallets::send_all` refuse a token's asset tag.
- Token-2022 mints are refused at `connect`, because their confidential
  transfers need not report an amount and would index as zero.
- The node must serve the Memo-v3 program, and `connect` refuses one that does
  not. Each transfer carries a memo with a random token, so two transfers of the
  same amount to the same address are distinct transactions.

# chain-solana

Solana for the payment SDK: it indexes finalized blocks into history and
balances for SOL and SPL tokens, and builds, signs and submits native SOL
transfers. The `native` module is the protocol code alone and can be used
without the rest of the SDK.

## Usage

```rust,no_run
use std::sync::Arc;

use chain_solana::{Config, Network, Store};
use payments::config::{ComposedChain, Rpc};
use wallets::SubmissionRegistrar;

/// `repository` is the one store every network is indexed into.
async fn solana<R: Store>(
    repository: R,
    registrar: &Arc<dyn SubmissionRegistrar>,
) -> Result<ComposedChain<&'static str>, Box<dyn std::error::Error>> {
    let genesis = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d".parse()?;

    let config = Config::new(
        Network::new("mainnet", genesis),
        Rpc::endpoint("http://127.0.0.1:8899"),
        "sol",
    );

    // Checks the genesis hash and every token mint. Opens and writes nothing.
    let chain = config.connect().await?;
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

- Configure one or more RPC endpoints. No request is ever retried or repeated
  by the transport, whatever `repeat` says. A read reaches one node and
  carries the previous read's slot as its floor, so a node behind it fails the
  send before anything is submitted. A submission is broadcast to every
  configured node, each exactly once; the first acceptance answers, and a
  failure on every node where one node did not answer stays an unknown
  outcome. Set each endpoint's timeout with that in mind.
- The genesis hash is configured and verified, never discovered. `connect`
  refuses a node that reports another hash, so a wrong URL cannot index a
  different cluster into the same store.
- A signed transaction expires with its blockhash. A send rebroadcasts the same
  signed bytes to every configured node until a node sees it land or the
  blockhash expires; it is never rebuilt or re-signed. Not seen landed before
  expiry fails as `WalletErrorKind::OutcomeUnknown` with the transaction ID: it
  may still have landed, so check history. The sender is reserved until then; a
  second send from it fails with `SourceBusy` instead of waiting.
- SPL tokens are received and reported, not sent. `Wallets::author` and
  `Wallets::send_all` refuse a token's asset tag.
- Token-2022 mints are refused at `connect`, because their confidential
  transfers need not report an amount and would index as zero.
- The node must serve the Memo-v3 program, and `connect` refuses one that does
  not. Each transfer carries a memo with a random token, so two transfers of the
  same amount to the same address are distinct transactions.

A native send accepts `wallets::Amount::All` as its source's final batch item.
The outbox resolves the bracketed balance after earlier transfers and fees,
rebuilds the final message with the same memo and lifetime, and verifies its
fee before signing and simulation. A zero remainder is refused. SPL assets
remain read-only; sweep does not provide token transaction authorship.

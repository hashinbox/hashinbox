# chain-solana

Solana for the payment SDK: reads blocks as `base` blocks with every leg of
SOL and the allowlisted SPL tokens, and builds, signs and broadcasts SOL and
token transfers.

## Usage

```rust,no_run
use base::{Asset, Broadcaster as _, BuilderExt as _, Chained as _, ServiceFactory as _, Storage, WalletFactory as _};
use std::sync::Arc;

use chain_solana::{Client, Config, Network};
use rpc::Endpoint;

async fn solana(storage: Storage) -> anyhow::Result<()> {
    let genesis = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d".parse()?;
    let config = Config::new(Network::new(genesis, "mainnet-beta"));
    let node: Arc<dyn rpc::Client> = Arc::new(rpc::node(&Endpoint::new("http://127.0.0.1:8899"), Default::default())?);
    // The one entry point a container uses: indexer, factory, broadcaster,
    // and assets over the application's stores.
    let _service = config.build(&storage, node.clone()).await?;

    let rpc = Client::new(node, config.native());

    // Reading: what `base::Indexer` consumes.
    let chain = config.chain(&rpc);
    let tips = chain.tips().await?;
    let _blocks = chain.blocks(tips.confirmed, tips.produced).await?;

    // Sending: sign, keep the bytes, then broadcast them to every node.
    let factory = config.factory(&rpc, std::sync::Arc::new(base::NoHoldings));
    let wallet = factory.from_pk(factory.generate().await?).await?;
    let sol = Asset::native((&config.native()).into());
    let mut builder = wallet.transaction(&sol)?;
    builder.send((factory.parse_address("...")?, 10_000_u64.into()))?;
    for transaction in builder.sign().await? {
        rpc.broadcast(&transaction).await?;
    }

    Ok(())
}
```

`Config::try_from(&base::NetworkConfig)` builds it from a stored network:
the genesis hash (`chain_id`), assets (SOL's display decimals, the
`contract` mints), `confirmations`, `max_history`, and from `settings` the
knobs `fee_ttl_ms`, `pending_ttl_ms`, `interval_ms`, and `batch`, the slots
the indexer fetches per step (default 100) (each absent one its default).

## Notes

- `Config` implements `base::ServiceFactory`: `build(&storage, client)` validates
  and returns the `base::Service`, asking no node. The indexer wakes every
  `interval_ms` milliseconds (default 1,000).
- The config holds no endpoints: `build(&storage, client)` builds every
  part over the given transport, asking no node. `info(nodes)` is the
  network's nodes as one client: each wrapped in `Info`, the chain's
  `base::InfoClient` (`tip()` is `getSlot` at `confirmed`, `info()` its chain and tip, asked
  once when a node is added), clustered by `tip()` (`base::Cluster`).
  `Client` extends `Info` with every other call and is an `InfoClient` too.
  Pass a `rpc::Swap` and keep it to change nodes live.
- Position is the slot; a skipped slot holds no block. `Header.height` is the
  block height. Blocks are read at `confirmed` commitment; `Tips.produced` is
  the newest confirmed slot, `Tips.confirmed` the newest rooted (`finalized`)
  one.
- `blocks` lists the produced slots with one `getBlocks`, then fetches them in
  JSON-RPC batches of ten `getBlock` calls. Only standard Solana methods are
  used.
- Legs come from each transaction's pre/post balances: lamports for every
  account, and units of each allowlisted mint netted per token-account owner.
  A decrease is `Spent`, an increase `Received`; the fee is a `Fee` leg of the
  fee payer, kept out of its `Spent`. A failed transaction keeps only its fee
  and is `Reverted`. Block rewards (rent, staking, fees) are signed rewards.
  Mints and burns appear as balance changes, not `Minted`/`Burned` legs.
- A builder sends one asset: SOL, or one configured token (`TransferChecked`,
  creating the recipient's associated token account when missing; the sender
  pays its rent, which the pending legs do not show); any other asset is
  `Unsupported`. It packs as many transfers as fit a packet into each
  transaction, all on one recent blockhash, each with a random Memo-v3 so two
  equal transfers stay distinct. Recipients must be on-curve addresses.
- The factory's one fee estimator (`WalletFactory::fees`), which builders
  sign on too (its blockhash and price), is two: a precise one (`fee.rs`)
  that asks the node every time (a blockhash, the price of a signature on it,
  and, for a token, whether the recipient has its token account and the
  rent of one), and a cached one (`fee_cache.rs`) around it, which keeps the
  blockhash and price for `fee_ttl_ms` (default 5 s) and the rent for good,
  and takes a recipient that `Holdings` say holds the token to have its
  account, without asking. An estimate is one signature per source plus,
  for a token, a token account's rent when the recipient has none (without
  a recipient, a new one has none); the `Fee` leg is the signature part only.
- `broadcast` sends the exact bytes once to every node, with preflight. A
  signed transaction lands only until its blockhash expires (about a minute);
  after that `broadcast` fails as `Expired`. Resending until then is the
  caller's job. A refusal as "already been processed" is acceptance.
- Token-2022 mints are refused at `connect`.

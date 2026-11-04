# chain-solana

Solana for the payment SDK: reads blocks as `base` blocks with every leg of
SOL and the allowlisted SPL tokens, and builds, signs and broadcasts SOL and
token transfers.

## Usage

```rust,no_run
use base::{Asset, Broadcaster as _, BuilderExt as _, Chained as _, Config as _, Storage, WalletFactory as _};
use chain_solana::{Config, Network};
use rpc::Endpoint;

async fn solana(storage: Storage) -> anyhow::Result<()> {
    let genesis = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d".parse()?;
    let config = Config::new(
        Network::new(genesis, "mainnet-beta"),
        vec![Endpoint::new("http://127.0.0.1:8899")],
    );
    // The one entry point a container uses: indexer, factory, broadcaster,
    // and assets over the application's stores.
    let _service = config.build(&storage, config.client().await?).await?;

    // Checks the genesis hash, the Memo-v3 program and every mint's decimals.
    let rpc = config.connect().await?;

    // Reading: what `base::Indexer` consumes.
    let chain = config.chain(&rpc);
    let tips = chain.tips().await?;
    let _blocks = chain.blocks(tips.confirmed, tips.produced).await?;

    // Sending: sign, keep the bytes, then broadcast them to every node.
    let factory = config.factory(&rpc);
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
the genesis hash (`chain_id`), endpoints, assets (SOL's display decimals, the
`contract` mints), `confirmations`, `max_history`, and from `settings` the
knobs `fee_ttl_ms`, `pending_ttl_ms`, `interval_ms` (each absent one its
default).

## Notes

- `Config` implements `base::Config`: `build(&storage, client)` validates,
  verifies the node and returns the `base::Service`. The indexer wakes every
  `interval_ms` milliseconds (default 1,000).
- `client()` is the chain's own cluster over `endpoints`; `build(&storage,
  client)` verifies once that `client` serves the network and builds every
  part over it. Pass a `rpc::Swap` and keep it to change nodes live.
  Endpoints are needed only by `client()`.
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
- The factory's one fee estimator (`WalletFactory::fees`) caches a blockhash
  and the price of a signature for `fee_ttl_ms` (default 5 s); builders sign
  on them, a message's fee being its signatures at that price. An estimate is
  one signature per source plus, for a token, a token account's rent (the
  recipient may lack one); the `Fee` leg is the signature part only.
- `broadcast` sends the exact bytes once to every node, with preflight. A
  signed transaction lands only until its blockhash expires (about a minute);
  after that `broadcast` fails as `Expired`. Resending until then is the
  caller's job. A refusal as "already been processed" is acceptance.
- Token-2022 mints are refused at `connect`.

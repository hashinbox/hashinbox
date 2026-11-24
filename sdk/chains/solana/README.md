# chain-solana

Solana for the payment SDK: reads blocks as `base` blocks with every leg of
SOL and the allowlisted SPL tokens, and builds, signs and broadcasts SOL and
token transfers.

## Usage

```rust,no_run
use base::{
    Asset, Broadcaster as _, Chained as _, ServiceFactory as _, Storage, Transfer,
    WalletFactory as _,
};
use std::sync::Arc;

use chain_solana::{Client, Config, Factory, Network};
use rpc::Endpoint;

async fn solana(storage: Storage) -> anyhow::Result<()> {
    let genesis = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d".parse()?;
    let config = Config::new(Network::new(genesis, "mainnet-beta"));
    let node: Arc<dyn rpc::Client> = Arc::new(rpc::node(&Endpoint::new("http://127.0.0.1:8899"))?);
    let rpc = Client::new(node.clone(), config.native());
    let sol = Asset::native((&config.native()).into());
    let factory = Factory::from(config);

    // What `base::ServiceFactory::service` builds a stored network into:
    // indexer, wallet factory, broadcaster and assets over the application's stores.
    let _service = factory.service(&storage, node)?;

    // Reading: what `base::Indexer` consumes.
    let chain = factory.chain(&rpc);
    let tips = chain.tips().await?;
    let _blocks = chain.blocks(tips.confirmed(chain.confirmations()), tips.position).await?;

    // Sending: one transfer signed into one transaction; keep the bytes,
    // then broadcast them to every node.
    let wallets = factory.wallets(&rpc, Arc::new(base::NoHoldings));
    let wallet = wallets.from_pk(wallets.generate().await?).await?;
    let mut builder = wallet.transaction(&sol)?;
    let to = wallets.parse_address("...")?;
    builder.from_transfer(Transfer::new(to, 10_000_u64.into()))?;
    let transaction = builder.sign().await?;
    rpc.broadcast(&transaction).await?;

    Ok(())
}
```

`Config::try_from(&base::NetworkConfig)` builds it from a stored network:
the genesis hash (`chain_id`), assets (SOL's display decimals, the
`contract` mints), `confirmations`, and from `settings` the knobs
`fee_ttl_ms`, `max_priority_fee` (micro-lamports per compute unit, default
1,000,000), `interval_ms`, `batch`, the slots the
indexer fetches per step (default 100), `read_depth`, the slots below the
tip the indexer reads (default 1,000, about 7 minutes), and the
`base::Tracking` of sent
payments: `check_ms` (default 10 s) and `max_bumps` (default 5; a payment
is bumped once it expired unseen, so `bump_after` is unused) (each absent
one its default). It
has no `max_depth`: it reads balances (`base::Balances`, finalized, at a
slot at or above the one asked: `minContextSlot`). Its line is
`Line::Depth(read_depth)`, the tip less `read_depth`, not the block after
the confirmed slot, so confirmed blocks are read too (`read_depth` is at
least `confirmations`). An index more than `read_depth`
slots behind, and a new one, settles with one balance read (design 083
§9).

## Notes

- `Config` is data; `Factory` builds from it. As the chain's
  `base::ServiceFactory`, `Factory::new(&network_config)` reads a stored
  network as its `Config`; `Factory::from(config)` takes one made in code.
  `factory.chain(&rpc)` is the block reader, `factory.wallets(..)` the
  wallet factory, and `factory.service(&storage, client)` validates and
  returns the whole `base::Service`, every part over the given client,
  asking no node. The indexer wakes every `interval_ms` milliseconds
  (default 1,000).
- The config holds no endpoints. `Info` is the chain's `base::InfoClient`
  over one node (`Info::new(node)`): `tip()` is `getSlot` at `confirmed`, `info()`
  its genesis hash and its tip, as the node reports them; the caller judges them.
  `Client` extends `Info` with every other call.
- Position is the slot; a skipped slot holds no block. `Header.height` is the
  block height. Blocks are read at `confirmed` commitment; `Tips.position` is
  the newest confirmed slot (`getSlot`); the confirmed slot is `confirmations`
  (default 32) below it (`Tips::confirmed`).
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
  `Unsupported`. It signs one transfer into one transaction on a recent
  blockhash, with a random Memo-v3 so two equal transfers stay distinct.
  Recipients must be on-curve addresses.
- Every transaction starts with two ComputeBudget instructions: a
  compute-unit limit, fixed per kind (40,000 for SOL, 100,000 for a token,
  about twice what each uses: the memo alone takes some 16,000, creating a
  token account about 25,000), and a price per unit, in micro-lamports.
  The rate (`base::FeeRate`) has no base; its tip is the median of
  `getRecentPrioritizationFees` for the whole network, at least 1
  micro-lamport, kept `fee_ttl_ms` (default 5 s) in one
  `base::FeeRateProviderCached` shared by every builder, the estimator and
  `Service::rates`; the price is the tip of the builder's `base::Fee` over
  it (levels × 0.5, 1, 2). Above `max_priority_fee` signing fails as
  `FeeCap`.
- A builder's saved state (`save`) is its payment, source, compute-unit
  limit and rate; `load` continues it. `Fee::Times(1.1)` then pays a tenth
  (rounded up, at least one micro-lamport) over the higher of the rate now
  and the saved one. `sign` takes a fresh blockhash every time, so a
  version has its own `expires_height`. Sign one only once the old one
  expired unseen: nothing else keeps both from landing.
- The factory's one fee estimator (`WalletFactory::fees`), which builders
  sign on too (its signature price), is `Estimator` (`fee_estimator.rs`):
  it asks the node (the price of a signature on a recent blockhash, and,
  for a token, the rent of a token account) and keeps the signature price
  for `fee_ttl_ms` and the rent for good, and takes a recipient that
  `Holdings` say holds the token to have its account, without asking. An estimate is per source one signature and its priority
  fee at the rate now, plus, for a
  token, a token account's rent when the recipient has none (without a
  recipient, a new one has none); the `Fee` leg is the signature and
  priority part, `ceil(price × limit / 1,000,000)` lamports.
- `broadcast` sends the exact bytes once to every node, with preflight. A
  signed transaction lands only until its blockhash expires (about a minute);
  after that `broadcast` fails as `Expired`. Resending until then is the
  caller's job. A refusal as "already been processed" is acceptance.
- A signed transaction carries `expires_height`, its blockhash's
  `lastValidBlockHeight`: once a caller has read past that height without
  finding it, no later block can take it.
- Its line is `read_depth` slots below the tip (`Line::Depth`). A sent
  transaction's index left behind it is settled by the `Confirmer`: its
  signature's status (`getSignatureStatuses` with history), finalized at
  or below the confirmed slot, is where it is found.
- Token-2022 mints are refused at `connect`.

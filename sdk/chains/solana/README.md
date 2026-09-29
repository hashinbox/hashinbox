# chain-solana: Solana support for the payment SDK

Indexes finalized Solana blocks into canonical history — native SOL and SPL
token movements alike — and builds, signs, and submits native SOL transfers.
It implements the SDK contracts (`indexing`, `wallets`, `base`) for Solana; it
does not own storage, the sync loop, or the HTTP API. The layout follows `docs/CHAIN_LAYOUT.md`: every file has the same
name and role as in `chain-ethereum`.

## What this chain provides

| SDK contract | This crate's type | Module |
|---|---|---|
| canonical `base::Address` / `TransactionRef` / `AssetId` | [`Address`], [`native::TransactionId`], [`AssetKind`] | `identity` (types in `native`) |
| `indexing::IndexedBlock` | [`native::block::RawBlock`] | `source` |
| `indexing::BlockSource` | [`Source`] | `source` |
| `indexing::BlockInterpreter` | [`BlockInterpreter`] | `interpreter` |
| `indexing::{Indexing, Checkpoint}`, and the history reader it hands out | [`Indexer`] (a type alias for `indexing::Service`) via [`indexer`] | `indexer` |
| `base::Signer` | [`KeyPair`] | `key` (type in `native`) |
| `wallets::{WalletFactory, FactoryProfile}` | [`WalletFactory`] with [`WalletConfig`] | `wallet` |
| `wallets::AddressParser` (user address text -> canonical `base::Address`) | [`WalletConfig`]; [`WalletFactory`]'s impl delegates to it, so a caller holding only a [`Network`] validates text without a store, a node, or an index | `wallet` |
| `base::Units` (what an amount of each configured asset is) | `Config::units` | `config` |
| `wallets::Wallet` = `indexing::Holder` + `base::Signer` | the factory's wallets | `wallet` |
| `wallets::Author` = `wallets::Wallet` + `SingleSender` + `TransactionFactory`, and `base::Broadcaster` | the same wallets, reached only through `Wallets::author` | `wallet` |
| `base::TransactionBuilder` (`transfer`, `draft`, `prepare`) | [`TransferBuilder`] | `builder` |
| `wallets::Authorship` (the batch `Sender` and the `Author` upgrade) | `FactoryProfile::authorship` of [`WalletFactory`] — `Some` for SOL, **`None` for an SPL mint**, which is the one place this integration says it does not send tokens | `sender` |
| submission supervision | `base::{SubmissionRegistrar, SubmissionTask}` (re-exported by `wallets`) | `sender` |
| native amount / asset | [`Lamport`], [`AssetKind`], [`SOL`] | `native` |
| SPL token positions and program decoding | [`native::block::TokenBalance`], [`native::spl`] | `native` |

Every failure is one [`Error`]; `error.rs` converts it into `SourceError`,
`IndexError`, `TransactionError`, or `wallets::Error`.

## Chain-specific model

- **Account model.** There are no UTXOs, and a reported balance is not a node
  read: `indexing::Balances::balance` folds it from the movements the index
  holds, at the same confirmed line the same store's history is read at. One
  [`WalletConfig`] names either native SOL or one SPL mint with its decimals
  ([`WalletConfig::with_token`]); [`WalletFactory::sibling`] gives a second
  asset its own factory over the same cluster, RPC, and send pipeline.
- **Position = slot.** `BlockPosition` is the slot and `BlockHeight` the
  produced-block height; slots may be empty. The source reads only finalized
  (rooted) blocks, so its tip is already its confirmed line: nothing is ever
  held above it, there is no confirmation number and no rollback journal, and
  [`IndexerParams::SYNC`] is one bound — batches of a hundred blocks.
- **Evidence.** A [`native::block::RawBlock`] keeps the exact JSON of
  `getBlock`; its header is checked when fetched. [`native::block::RawBlock::parse`]
  is the only parser: a `Transaction` carries its account keys with
  before/after balances, its token positions from
  `meta.pre/postTokenBalances`, and its instructions with their inner
  instructions, and `Transaction::movements` lists every
  decoded System and SPL Token transfer, then the residual each account's
  lamport change and each position's unit change leaves unexplained. A node
  that reports no `innerInstructions`, or no token balances, therefore costs
  attribution — one residual per side instead of one named transfer — and
  never stops the chain.
- **Fee settlement.** The fee is charged outside the System transfer, so
  [`indexing::BlockInterpreter::settlement`] answers `FeeSettlement::Charged` and a
  balance fold debits it from the fee payer, in lamports only.
- **The residual is a balance fact.** `pre`/`postBalances` are the runtime's
  own accounting, so every lamport a CPI, an account close returning rent, or
  an instruction a node omitted from `innerInstructions` moves is exact and
  known — it just has no counterpart to name. Recording it one-sided rather
  than rejecting the block is what makes the folded balance equal the
  account's real delta; absorbing it would leave a balance quietly short by
  every contract payment the account ever received. `Transaction::reconcile`
  is therefore an assertion that the arithmetic is sound, not a filter on
  which value effects the chain is allowed to have (`docs/design/021`
  section 4.2).
- **No balance witness.** Solana cannot read an account at a past slot, so
  there is no independent oracle to pin against a fold at the confirmed
  checkpoint the way Ethereum pins `balanceOf` to a block hash. The block's
  own `rewards` array is not fetched (`"rewards": false`): epoch and staking
  rewards credit stake and vote accounts, which this SDK does not manage
  (`docs/design/021` sections 4.2 and 7.4).
- **SPL tokens.** A token balance entry names the *owner*, so a wallet's
  position is attributed without tracking its associated-token address; a side
  the transaction does not report is zero, because the token account was
  created or closed in it. The balance diff is the authority: SPL `Transfer`
  and `TransferChecked` are decoded to name both owners, and every other token
  effect — a mint, a burn, a Token-2022 fee, an instruction this crate does not
  read — falls out as a one-sided `:token:<account index>` residual, exactly as
  lamports do. Decimals belong to the mint account, not to configuration:
  `RpcClient::validate_token` reads it at startup and refuses a mismatch, a
  non-mint, or a Token-2022 mint, whose confidential transfers need not report
  an amount. An SPL wallet receives, reports its balance, and serves its
  history; it authors nothing. Every way in refuses with `Unsupported` — the
  single send, the batch, and the transaction builder, which rejects at
  `transfer` and at `resume` — rather than move SOL under a token's asset tag.
- **Network.** A [`Network`] is a slug plus the cluster's [`GenesisHash`]; the
  client refuses a node that reports another genesis or lacks the
  executable Memo-v3 program.
- **Sending.** Each transfer is one legacy transaction: a System transfer and
  a Memo-v3 instruction with a random token. A send reserves its sender
  until it settles; a second send from a busy sender fails with
  `SourceBusy`. Fees come from `getFeeForMessage`; every transaction is
  simulated before any is submitted. A submission whose outcome is unknown
  keeps the sender reserved until the node or the index proves the
  transaction present, or its blockhash expired unseen. That recovery runs
  as a `base::SubmissionTask`: by default on the current Tokio runtime, or
  under the application's `base::SubmissionRegistrar`
  ([`WalletFactory::with_registrar`]).
  It is the only index read of [`WalletFactory::new`]: the [`Indexer`]'s
  checkpoint, its commit announcements, and the sender's SOL history. Wallet
  history (SOL, with SOL fees) is read through the `indexing::History` reader
  the composition hands back, never off the indexer.
- **Keys.** A [`KeyPair`] is an Ed25519 seed wrapping a `crypto::KeyPair`,
  the same signing engine Bitcoin and Ethereum use on their own curve; like
  the address, it names no cluster. The address *is* the seed's Ed25519 public
  key, so every signature verifies under it by construction.

## Initialization

`connect` verifies the genesis hash and the Memo-v3 program, so a
[`RpcClient`] always serves the configured cluster. Everything after it is
synchronous and does no I/O, except `RpcClient::validate_token`, which reads
each configured mint before its factory exists.

```rust,no_run
use std::sync::Arc;

use chain_solana::{
    IndexerParams, KeyPair, Network, RpcClient, WalletConfig, WalletFactory, indexer,
};

/// `open` opens the one store every network is indexed into.
async fn run<R>(open: impl FnOnce() -> R) -> Result<(), Box<dyn std::error::Error>>
where
    // What this chain needs of a store: the two ports any index requires, plus
    // the reads its own consumers make.
    R: chain_solana::Store,
{
    let network = Network::new("localnet", "11111111111111111111111111111111".parse()?);
    // One node, one attempt per call: every Solana request is `Request::once`,
    // so no decorator could repeat it anyway.
    let endpoint = json_rpc::Endpoint::new("http://127.0.0.1:8899").with_bounds(
        json_rpc::Bounds { max_request_bytes: 1_048_576, ..json_rpc::Bounds::default() },
    );
    let transport = endpoint.connect(&std::collections::BTreeMap::new())?;
    let rpc = RpcClient::connect(transport, &network).await?;

    let repository = open();
    // The store is the durable half of the watch list too, so a restart
    // resumes with the observations it had.
    let indexer = Arc::new(indexer(IndexerParams::new(
        rpc.clone(),
        repository,
    ))?);

    // The sender's absence proof: the repository's own confirmed pages joined
    // to this index's overlay. Neither half answers it alone, and this is the
    // one value that holds both.
    let history = Arc::new(indexer.history());
    let settings = WalletConfig::new(&network);
    let factory = WalletFactory::new(settings.clone(), &rpc, &indexer, history)?;
    let wallet = factory.attach(KeyPair::generate()?)?;

    // One SPL token beside SOL: the mint's decimals are checked against the
    // mint account, and the sibling shares this factory's send pipeline.
    let mint = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU".parse()?;
    rpc.validate_token(&mint, 6).await?;
    let usdc = factory.sibling(settings.with_token(mint, 6))?;

    let mut wallets = wallets::Wallets::<&str>::new();
    wallets.add("sol", factory)?;
    wallets.add("usdc", usdc)?;
    // The map keeps no wallet: the index observes it, and the caller holds it.
    wallets.accepts(&"sol", wallet.as_ref())?;
    indexing::Watching::watch(
        indexer.as_ref(),
        "alice",
        wallet.as_ref(),
        indexing::Start::At(base::BlockPosition(0)),
    )
    .await?;
    Ok(())
}
```

A store takes `&impl base::Networked`, so the native `Network` is the only
identity a caller names; its `canonical()` is `solana/localnet`, the key the repository
is opened with. Override policy with
struct update syntax, e.g. `IndexerParams { sync, ..IndexerParams::new(rpc, repo) }`,
and supervise submissions with
`WalletFactory::new(config, &rpc, &indexer)?.with_registrar(registrar)`.
Every RPC call is one attempt: set the transport's timeout and size limits,
and expect no transparent retry.

### From a configuration value

[`Config`] is this chain's whole deployment surface, and it is an ordinary
value: build it in code or deserialize it from any `serde` format, and both
reach the same defaults and the same [`Config::validate`]. `connect` verifies
the node's identity and opens nothing; the verified [`Node`] then composes over
a repository into a [`wallets::ComposedChain`] — the index, the factories under
their asset tags, and this chain's own polling cadence. That type is declared
once, in `wallets`, and every chain returns it, so [`wallets::Composition`]
holds this chain beside the others without knowing which chains they are. The
asset tag is a type parameter, so an application keys its wallets by whatever
it already uses.

```rust,ignore
let cluster = Network::new("localnet", genesis_hash);
let chain = Config {
    tokens: vec![Token::new(mint, "usdc", 6)],
    ..Config::new(cluster, Rpc::endpoint(url), "sol")
};
let node = chain.connect(&secrets).await?;   // genesis hash and every mint
let composed = node.compose(repository, &registrar).await?;   // registrar: solana uses it
```

No file and no TOML are involved: `sdk-config` supplies [`sdk_config::Rpc`]
and [`sdk_config::Cadence`] as plain data, and its document reader is a
separate, optional convenience this crate never links. The other way in is
registration: [`Config`] implements [`sdk_config::Chain`], so an application
that does read a document writes
`registry.register::<Config<Tag>>("solana")` once and never names this crate
again. Parsing is sugar layered on the value; the value is the API.

## Native vs. adapter modules

[`native`] is protocol code: JSON-RPC (`native::rpc`), block parsing with
System- and SPL Token-transfer decoding (`native::block`, `native::spl`),
keys and addresses, lamports,
the Memo token, and transfer building and signing with their rules
(`native::transaction`: blockhash lifetime, batch funds, preflight). It never
names `indexing` or `wallets` and implements no SDK trait, so it is usable on
its own for raw node access.

Everything else in `src/` is adapter code with one file per SDK contract. It
maps native types to canonical ones and never talks to the node directly;
`sender/` is the send pipeline (`Outbox`) the factory's wallets share:
sender reservations, submission tasks, and recovery of unknown outcomes.

## Examples and related crates

Runnable programs: every [`examples/`](../../../examples/README.md) scenario sets this chain
up beside the others (`cargo run -p payment-examples --example wallet`, then `transfer`,
`indexing`, `watch_only`, and `all_chains` for the composition root in miniature).

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | `crypto`, `json-rpc` | keys and hashing; RPC transport under [`RpcClient`] |
| base | `base` | canonical values and signing/broadcast capabilities this crate implements |
| indexing | `indexing` | block source, interpreter, and indexer contracts |
| wallets | `wallets` | wallet factory and sender contracts; `Wallets` composes this crate's factory |
| downstream | `payment-server`, `payment-examples` | compose this crate; nothing else depends on it |

Design notes: [`docs/CHAIN_LAYOUT.md`](../../../docs/CHAIN_LAYOUT.md),
[`docs/INDEXING.md`](../../../docs/INDEXING.md), [`ARCHITECTURE.md`](../../../ARCHITECTURE.md).

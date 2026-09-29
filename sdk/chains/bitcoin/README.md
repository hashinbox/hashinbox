# chain-bitcoin: Bitcoin support for the payment SDK

Indexes Bitcoin blocks into canonical history and spendable outputs, and
builds, signs, and submits BTC transfers from P2WPKH and P2TR wallets. It
implements the SDK contracts (`indexing`, `wallets`, `base`) for Bitcoin; it
does not own storage, the sync loop, or the HTTP API. The layout follows
`docs/CHAIN_LAYOUT.md`: every file has the same name and role as in
`chain-ethereum`.

## What this chain provides

| SDK contract | This crate's type | Module |
|---|---|---|
| canonical `base::Address` / `TransactionRef` / `OutputId` | [`Address`], [`native::TransactionId`], [`native::block::Outpoint`] | `identity` (types in `native`) |
| `indexing::IndexedBlock` | [`native::block::RawBlock`] | `source` |
| `indexing::BlockSource` | [`Source`] | `source` |
| `indexing::BlockInterpreter` | [`BlockInterpreter`] | `interpreter` |
| `indexing::{Indexing, Checkpoint}`, and the two query readers it hands out | [`Indexer`] (a type alias for `indexing::Service`) via [`indexer`] | `indexer` |
| `base::Signer` | [`KeyPair`] | `key` (type in `native`) |
| `wallets::{WalletFactory, FactoryProfile}` | [`WalletFactory`] with [`WalletConfig`] | `wallet` |
| `wallets::AddressParser` (user address text -> canonical `base::Address`) | [`WalletConfig`]; [`WalletFactory`]'s impl delegates to it, so a caller holding only a [`Network`] validates text without a store, a node, or an index | `wallet` |
| `base::Units` (what an amount of each configured asset is) | `Config::units` | `config` |
| `wallets::Wallet` = `indexing::Holder` + `base::Signer` | the factory's wallets | `wallet` |
| `wallets::Author` = `wallets::Wallet` + `SingleSender` + `TransactionFactory`, and `base::Broadcaster` | the same wallets, reached only through `Wallets::author` | `wallet` |
| `base::TransactionBuilder` (`transfer`, `draft`, `prepare`) | [`TransferBuilder`] | `builder` |
| `wallets::Authorship` (the batch `Sender` and the `Author` upgrade) | `FactoryProfile::authorship` of [`WalletFactory`] — `Some` for every asset this chain carries | `sender` |
| native amount / asset / fee rate | [`Satoshi`], [`BTC`], [`FeeRate`] | `native` |

Every failure is one [`Error`]; `error.rs` converts it into `SourceError`,
`IndexError`, `TransactionError`, or `wallets::Error`.

## Chain-specific model

- **UTXO model.** There is no node balance read. [`BlockInterpreter`]
  materializes a selected address's unspent outputs; `Indexer::outputs()` hands
  out the `indexing::ObservedOutputs` that serves them — the store's rows with
  the pending overlay folded in, so change is spendable at depth one — and that
  is the only index read [`WalletFactory::new`] takes, as an
  `Arc<dyn indexing::OutputStore>`. A query is not a capability of the index
  itself: balance and history (BTC, with BTC fees) are read the same way, off
  the value that owns the rows rather than off the thing indexing them.
- **Fee settlement.** The interpreter emits every spent input and every
  created output, so the fee is already `Σ inputs − Σ outputs`.
  [`indexing::BlockInterpreter::settlement`] therefore answers
  `FeeSettlement::Movements`, and the balance fold does *not* apply
  `NetworkFee` again — doing so would charge every transaction's fee twice.
- **No block-level credit and no balance witness.** Every satoshi that reaches
  an address is an output of a transaction, the coinbase included, so there is
  nothing a block moves that no transaction names — unlike Ethereum's
  consensus-layer withdrawals. There is also nothing to check a fold against:
  Core keeps no address index, and this crate uses no `scantxoutset` and no
  wallet RPC, so a balance here is a fold over indexed rows and a policy about
  where watching started, never a proof (`docs/design/021` sections 4.3 and
  7.4).
- **Position = height.** `BlockPosition` and `BlockHeight` are both the block
  height. [`IndexerParams::SYNC`] waits for 6 confirmations and can roll back
  100 blocks.
- **Evidence.** A [`native::block::RawBlock`] keeps Bitcoin Core's `getblock`
  verbosity-2 JSON with each spent output resolved into a compact `prevout`
  (value and address), plus its [`Network`]. [`native::block::RawBlock::parse`]
  is the only parser; each `Transaction` input carries its previous output,
  and `Transaction::movements` lists spent inputs and created outputs.
- **Keys.** A [`KeyPair`] wraps a `crypto::KeyPair` over a secp256k1 scalar
  and names no network. [`WalletFactory::attach`] derives the address from the
  configuration's network and [`AddressType`].
- **Fees.** [`FeeRate`] is sat/kvB, Bitcoin Core's unit; build it with the unit
  in the name ([`FeeRate::from_sat_per_vb`]). Wallets estimate with
  `estimatesmartfee` and refuse a rate above [`WalletConfig`]'s ceiling,
  which defaults to Core's own `maxfeerate` ([`FeeRate::DEFAULT_MAXIMUM`],
  0.1 BTC/kvB) and is also passed to `testmempoolaccept` and
  `sendrawtransaction`.
- **Sending.** A send is one transaction. One sender selects its largest
  outputs first and gets change; a batch from several senders spends all of
  each sender's free outputs, and each pays its own transfers, change, and
  share of the fee. Selection reserves the chosen outputs, so concurrent
  sends never pick the same one; they stay reserved until the index shows
  them spent or the node refuses the transaction. A submission whose outcome
  is unknown keeps them; the next broadcast of it, or the next send from its
  address, asks the node for it or replays the identical bytes. Change of an
  in-flight transaction is not spendable until it is in a block; from then it
  is, at depth one, because selection reads the confirmed outputs with the
  blocks above the confirmed line folded in.

## Initialization

`connect` verifies the node (Bitcoin Core 31.x, unpruned, synchronized, with
`txindex`) serves the configured network, so a [`RpcClient`] always serves
it. Everything after it is synchronous and does no I/O.

```rust,no_run
use std::sync::Arc;

use chain_bitcoin::{
    IndexerParams, KeyPair, Network, RpcClient, WalletConfig, WalletFactory, indexer,
};

/// `open` opens the one store every network is indexed into.
async fn run<R>(open: impl FnOnce() -> R) -> Result<(), Box<dyn std::error::Error>>
where
    // What this chain needs of a store: the two ports any index requires, plus
    // the reads its own consumers make.
    R: chain_bitcoin::Store,
{
    let network = Network::Regtest;
    // The node's own URL, verbatim; `${NAME}` references resolve through
    // `json_rpc::Secrets`, which only a composition root implements.
    let endpoint = json_rpc::Endpoint::new("http://user:pass@127.0.0.1:18443");
    let transport = endpoint.connect(&std::collections::BTreeMap::new())?;
    let rpc = RpcClient::connect(transport, &network).await?;

    let repository = open();
    // The store is the durable half of the watch list too, so a restart
    // resumes with the observations it had.
    let indexer = Arc::new(indexer(IndexerParams::new(
        rpc.clone(),
        repository,
    ))?);

    // The index hands out the `indexing::OutputStore` coin selection reads: the
    // store's own unspent rows with this index's pending overlay folded in.
    let outputs = Arc::new(indexer.outputs());
    let factory = WalletFactory::new(WalletConfig::new(&network), &rpc, outputs)?;
    let wallet = factory.attach(KeyPair::generate()?)?;

    let mut wallets = wallets::Wallets::<&str>::new();
    wallets.add("btc", factory)?;
    // The map keeps no wallet: the index observes it, and the caller holds it.
    wallets.accepts(&"btc", wallet.as_ref())?;
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
identity a caller names; its `canonical()` is `bitcoin/regtest`, the key the repository
is opened with. Override policy with
struct update syntax, e.g.
`IndexerParams { confirmed_depth: 12, ..IndexerParams::new(rpc, repo) }`,
and `WalletConfig::new(&network).with_address_type(AddressType::Taproot)
.with_max_fee_rate(FeeRate::from_sat_per_vb(500))`. A deployment that assembles
itself from a [`Config`] makes the same two choices there, as `address_type`
and `limits`, and never names [`WalletConfig`]: see the next section.

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
let chain = Config {
    confirmation_depth: 100,                 // this deployment's own safety trade
    address_type: AddressType::Taproot,      // which addresses its wallets derive
    limits: Limits { max_fee_rate: 500_000, ..Limits::default() },  // sat/kvB ceiling
    cadence: Cadence { policy: SyncPolicy::new(250), ..Cadence::new() },
    ..Config::new(Network::Regtest, Rpc::endpoint(url), "btc")
};
let node = chain.connect(&secrets).await?;   // identity only; nothing is opened
let composed = node.compose(repository, &registrar).await?;
```

No file and no TOML are involved: `sdk-config` supplies [`sdk_config::Rpc`]
and [`sdk_config::Cadence`] as plain data, and its document reader is a
separate, optional convenience this crate never links. The other way in is
registration: [`Config`] implements [`sdk_config::Chain`], so an application
that does read a document writes
`registry.register::<Config<Tag>>("bitcoin")` once and never names this crate
again. Parsing is sugar layered on the value; the value is the API.

## Native vs. adapter modules

[`native`] is protocol code: Bitcoin Core JSON-RPC (`native::rpc`), block
parsing (`native::block`), previous-output resolution (`native::prevout`),
keys and addresses, amounts, and building and signing with their rules
(`native::transaction`: preflight, grouped funds). It never names `indexing`
or `wallets` and implements no SDK trait, so it is usable on its own for raw
node access.

Everything else in `src/` is adapter code with one file per SDK contract. It
maps native types to canonical ones and never talks to the node directly;
`sender/` is the send pipeline (`Outbox`) the factory's wallets share:
output reservations, recovery of unknown outcomes, and `sender/utxos.rs`,
the index-backed UTXO view it spends from.

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

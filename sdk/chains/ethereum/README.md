# chain-ethereum: Ethereum support for the payment SDK

Indexes Ethereum blocks into canonical history and builds, signs, and submits
ETH and ERC-20 transfers. It implements the SDK contracts (`indexing`,
`wallets`, `base`) for Ethereum; it does not own storage, the sync loop, or
the HTTP API. The layout is the reference for every chain crate: see
`docs/CHAIN_LAYOUT.md`.

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
| `wallets::Authorship` (the batch `Sender` and the `Author` upgrade) | `FactoryProfile::authorship` of [`WalletFactory`] — `Some` for every asset this chain carries | `sender` |
| native amount / asset | [`Wei`], [`AssetKind`], [`ETH`] | `native` |

Every failure is one [`Error`]; `error.rs` converts it into `SourceError`,
`IndexError`, `TransactionError`, or `wallets::Error`.

## Node requirement: the RPC endpoint must trace blocks

**An Ethereum RPC endpoint that cannot trace blocks is not supported.** Indexing
requires

```text
debug_traceBlockByHash(<block hash>, {"tracer": "prestateTracer",
                                      "tracerConfig": {"diffMode": true}})
```

and is called once per non-empty block, beside `eth_getBlockByNumber` and the
receipts. Ether moved by a contract — a router or splitter forwarding a payment,
an exchange proxy or escrow paying out, a smart-account or multisig execution, a
`SELFDESTRUCT` beneficiary credit, the value leg of any `CALL` — appears in no
transaction field and in no log, so without the trace a wallet that is paid
through a contract is simply indexed wrong, and nothing notices. There is no
"transactions only" mode and no configuration flag to turn this off: a balance
that is silently incomplete is worse than a chain that refuses to start.

[`RpcClient::validate_tracing`] traces the node's own head at startup and
refuses a node that does not implement the method or that ignores `diffMode`,
naming the method and the fix. The composition root calls it beside the chain-ID
and ERC-20 checks, so a traceless node stops Ethereum only — Bitcoin and Solana
compose and index independently. If an endpoint loses the capability later, the
per-block failure is terminal rather than retried, which quarantines the
Ethereum network and leaves the other chains synchronizing.

What this costs, per block: one more call, roughly an order of magnitude more
payload, and a method most providers weigh at 50-300 plain calls. The binding
constraint is state, not price — a non-archive node can only trace blocks still
inside its state-retention window, which is what sets how far back Ethereum can
be backfilled.

ERC-20 needs none of this: a token transfer emits a `Transfer` log wherever it
originates, so the receipts already carry it.

## Chain-specific model

- **Account model.** Nonces come from the node (`eth_getTransactionCount`) and
  there are no UTXOs. A *reported* balance is no longer a node read at all:
  `indexing::Balances::balance` folds it from the movements the index holds,
  so it is folded at the same confirmed line the same store's history was read
  at. The node's balance reads survive only where an affordability check needs
  them, at [`native::rpc::View`]`::Pending`, which counts the sender's own
  in-flight transactions (`docs/design/021` section 5).
- **Position = height.** `BlockPosition` and `BlockHeight` are both the block
  number. [`IndexerParams::SYNC`] waits for 12 confirmations and can roll back
  100 blocks.
- **Evidence.** A [`native::block::RawBlock`] keeps the exact JSON of
  `eth_getBlockByNumber`, every receipt, and the block's prestate diff.
  [`native::block::RawBlock::parse`] is the only parser; a `Transaction`
  carries its `Receipt` and its [`native::block::Trace`], and
  `Transaction::movements` decodes ether value, ERC-20 `Transfer` logs, and the
  ether those two leave unexplained. Traces are held for one indexing pass and
  never written to storage.
- **Fee settlement.** Gas is charged outside the transferred value, so
  [`indexing::BlockInterpreter::settlement`] answers `FeeSettlement::Charged` and a
  balance fold debits the fee from its payer — in ether only, which is why an
  ERC-20 holding never pays gas and the same address's native holding always
  does.
- **Contract-initiated ether.** The traced balance of an account is the
  runtime's own accounting and therefore total; the transaction's own `value`
  and its logs are attribution over it. Whatever they do not explain becomes a
  one-sided `<txid>:residual:<address>` movement — a `Mint` for a credit, a
  `Burn` for a debit — attributed to the transaction that caused it. This is
  the rule Solana already uses for its `preBalances`/`postBalances`, so both
  account chains read one model. Two consequences worth stating: a plain
  transfer leaves nothing over and still produces exactly one movement, and the
  gas the sender paid is already inside the traced balance, so it is added back
  when residuals are computed and the transaction's `Fee` stays the only record
  of it.
- **Consensus-layer withdrawals.** Post-Shanghai a validator's partial
  withdrawals and full exits credit an execution address straight from the
  block body, with **no transaction, no receipt and no trace entry**. An
  address that has only ever received withdrawals therefore has an empty
  history and a non-zero balance, which makes this a balance fact rather than
  a history nicety. [`native::block::RawBlock::withdrawals`] reads them out of
  the `eth_getBlockByNumber` payload already held — no extra RPC call — as
  [`native::block::Withdrawal`], converting the body's Gwei to wei once, at
  the parse, because a missed factor of 10^9 does not look wrong in a number.
  They do not reach a balance yet: `indexing::InterpretedBlock` carries
  transactions and output changes, and a credit with no transaction to name
  is neither (`docs/design/021` section 4.4, `docs/design/030` step 13).
- **The balance witness.** The reported balance is the index's fold, always.
  [`WalletFactory::witness`] gives a [`BalanceWitness`] that asks one node,
  pinned to the *same* block the fold settled at (`blockHash` with
  `requireCanonical`), what it computes for the same holding —
  `eth_getBalance` for ether, `balanceOf` for a token — and returns an
  [`Attestation`]: `Agreed`, `Disagreed` with both amounts, or `Unavailable`.
  It never changes the number and it can never fail a balance read. Agreement
  is the only evidence in this SDK that promotes a fold to
  `indexing::Completeness::Complete`, because it is a claim about the *start
  position* and not merely about the arithmetic: a rebasing token, a mint not
  spelled as a `Transfer`, a proxy that changed its accounting, or an address
  funded before the index began watching are all invisible in logs and all
  visible here (`docs/design/021` sections 7.1-7.3). Ethereum is the only chain
  in this SDK that can do it: Bitcoin Core has no address index and Solana
  cannot read an account at a past slot, so neither has an oracle to pin.
- **Keys.** A [`KeyPair`] wraps a `crypto::KeyPair` over a secp256k1 scalar
  and derives its own address; it names no network and no asset.
- **Assets.** One [`WalletConfig`] selects ether or one ERC-20 token. Wallets of
  different assets that share an address must share one send pipeline: build
  the second factory with [`WalletFactory::sibling`]. A factory states which
  asset its wallets hold (`FactoryProfile::asset`) and nothing else about it:
  what an amount of that asset *is* belongs to the asset, and [`Config::units`]
  declares it — ether from [`ETH`], and every allowlisted token in the scale
  [`Config::connect`] checked its contract's own `decimals()` against.
  `wallets::Wallets` reads history from its own index, so the factory takes
  no index at all.
- **Sending.** Each transfer is one EIP-1559 transaction. A send reserves its
  senders until every transaction settles; a second send from a busy sender
  waits. Nonces start at the later of the node's pending nonce and the last
  nonce this process saw accepted; the whole batch is preflighted,
  balance-checked, and signed before the first broadcast. A submission whose
  outcome is unknown keeps its exact envelope, and the next send from that
  sender first asks the node for the hash or replays the identical bytes. A
  transaction signed through [`TransferBuilder`] but never broadcast holds
  nothing: a later send may take its nonce, and then its broadcast fails.
- **Fees.** EIP-1559 only. [`FeeLimits`] caps calldata, gas, and fees; a quote
  above a ceiling is rejected, never clamped.

## Initialization

`connect` verifies `eth_chainId`, so a [`RpcClient`] always serves the
configured chain; [`RpcClient::validate_tracing`] then proves it can trace, and
[`RpcClient::validate_token`] each allowlisted contract. Everything after them
is synchronous and does no I/O.

```rust,no_run
use std::sync::Arc;

use chain_ethereum::{
    IndexerParams, KeyPair, Network, RpcClient, WalletConfig, WalletFactory, indexer,
};

/// `open` opens the one store every network is indexed into.
async fn run<R>(open: impl FnOnce() -> R) -> Result<(), Box<dyn std::error::Error>>
where
    // What this chain needs of a store: the two ports any index requires, plus
    // the reads its own consumers make.
    R: chain_ethereum::Store,
{
    let network = Network::new(31_337, "anvil");
    // The node's own URL, verbatim; `${NAME}` references resolve through
    // `json_rpc::Secrets`, which only a composition root implements.
    // A composition root supplies the real `json_rpc::Secrets`; a map with no
    // references resolves a URL that names none.
    let secrets = std::collections::BTreeMap::new();
    let node = json_rpc::Endpoint::new("http://127.0.0.1:8545");
    let transport = node.connect(&secrets)?;
    let rpc = RpcClient::connect(transport, &network).await?;

    let repository = open();
    // The store is the durable half of the watch list too, so a restart
    // resumes with the observations it had.
    let indexer = Arc::new(indexer(IndexerParams::new(
        rpc.clone(),
        repository,
    ))?);

    let factory = WalletFactory::new(WalletConfig::new(&network), &rpc)?;
    let wallet = factory.attach(KeyPair::generate()?)?;

    let mut wallets = wallets::Wallets::<&str>::new();
    wallets.add("eth", factory)?;
    // The map keeps no wallet: the index observes it, and the caller holds it.
    wallets.accepts(&"eth", wallet.as_ref())?;
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
identity a caller names; its `canonical()` is `ethereum/anvil`, the key the repository
is opened with. Override policy with
struct update syntax, e.g. `IndexerParams { sync, ..IndexerParams::new(rpc, repo) }`,
and `WalletConfig::new(&network).with_token(usdc, 6).with_fee_limits(limits)`.

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
    tokens: vec![Token::new(usdc, "usdc", 6)],
    ..Config::new(Network::new(1, "mainnet"), Rpc::endpoint(url), "eth")
};
let node = chain.connect(&secrets).await?;   // chain ID, trace probe, every token
let composed = node.compose(repository, &registrar).await?;
```

No file and no TOML are involved: `sdk-config` supplies [`sdk_config::Rpc`]
and [`sdk_config::Cadence`] as plain data, and its document reader is a
separate, optional convenience this crate never links. The other way in is
registration: [`Config`] implements [`sdk_config::Chain`], so an application
that does read a document writes
`registry.register::<Config<Tag>>("ethereum")` once and never names this crate
again. Parsing is sugar layered on the value; the value is the API.

## Native vs. adapter modules

[`native`] is protocol code: JSON-RPC (`native::rpc`), block parsing
(`native::block`), ERC-20 ABI (`native::erc20`), keys, amounts, and transfer
building and signing with their rules (`native::transaction`: preflight,
nonces, batch funds). It never names `indexing` or `wallets` and implements no
SDK trait, so it is usable on its own for raw node access.

Everything else in `src/` is adapter code with one file per SDK contract. It
maps native types to canonical ones and never talks to the node directly;
`sender/` is the send pipeline (`Outbox`) the factory's wallets and its
siblings share: sender reservations, nonce floors, and recovery of unknown
outcomes.

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

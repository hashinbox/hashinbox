# wallets: chain-neutral wallet contracts, the `Wallets` factory map, and the composition root

## Why this crate exists

An application wants to create wallets, show balances and history, and send,
without importing a chain. This crate defines what a chain must provide for
that ([`WalletFactory`] and the capability traits its wallets implement) and
the one map that uses them: [`Wallets`], keyed by caller-chosen asset tags.

**It unifies factories and stores nothing else.** [`Wallets::create`] and
[`Wallets::from`] return `Arc<dyn Wallet>` and keep no copy, so addressing,
signing, balance and history are reachable without naming a chain crate — from
the wallet the caller holds. **Spending is not one of them.** A wallet of an
asset this system also spends is an [`Author`], which [`Wallets::author`] is
the only source of, and which a factory that declared no [`Authorship`] has
none of; sending and building are reachable from that, and from nothing else.
So an asset the system receives and reports — an SPL holding, say — is a type
that cannot author a payment, rather than one that refuses to. Three other
owners answer the rest:
`indexing` owns the durable list of what is observed
([`indexing::Watching::watch`]), the chain owns building and signing, and the
application owns ids and where keys live (`docs/design/030 §8 (ex-029)`).

There is no `parse_secret`: every supported chain's import format is 32 bytes
as 64 hexadecimal characters, which [`SecretBytes::from_hex`] already decodes
chain-neutrally, and the curve check belongs to [`WalletFactory::create`].
[`Wallets::secret`] makes a new key in the chain's own format and keeps
nothing, for callers that want the key before the wallet.

It also holds the **composition root**, because that is the one place where
`indexing::Index`, [`Wallets`] and [`WalletFactory`] must all be named at
once, and `linter.toml` gives this crate exactly those layers. What it no longer
holds is a store contract: the ports live in `indexing` (`indexing::Repository`
is the required pair), each chain crate names what *it* needs of a store, and
[`Composition::build`] takes one store value and hands every chain a clone of
it. [`Composition`] turns verified chains into a running system;
[`ComposedChain`] is what each chain hands back, declared here once rather than
once per chain crate.

It deliberately does not own keys at rest, a database, a ledger, deposit
accounting, an id map, or any chain rules.

## Concepts

| Concept | Type | Provided by |
|---|---|---|
| makes the wallets of one asset | [`WalletFactory`] (= [`AddressParser`] + [`FactoryProfile`]) | chain `WalletFactory` (`chain_*::wallet`) |
| validates address text with no store | [`AddressParser`] | chain `WalletConfig` — the factory's own impl delegates to it |
| a wallet | [`Wallet`] = [`Holder`] + `base::Signer` | the chain factory's wallets |
| a wallet of an asset the system spends | [`Author`] = [`Wallet`] + [`SingleSender`] + [`TransactionFactory`], from [`Wallets::author`] alone | the chain factory's wallets |
| "this asset is spent at all" | [`FactoryProfile::authorship`] -> `Option<`[`Authorship`]`>`, read once at registration | each chain's factory |
| holding one | the return of [`Wallets::create`] / [`Wallets::from`]; the caller keeps it | the caller |
| "this wallet is one of this asset's" | [`Wallets::accepts`], over [`Origin`] and [`Dependency`] | this crate |
| building a transaction | [`TransactionFactory::transaction`] -> `base::TransactionBuilder`, inspected as `base::Draft` and `base::SignedTransaction::fee` | the chain factory's wallets, reached through [`Author`] |
| batch sending | [`Wallets::send_all`], [`Sender`], [`Transfer`], [`SendError`] | chain `sender.rs` ([`Authorship`], which is a [`Sender`]) |
| the map | [`Wallets`], [`AssetInfo`] | this crate |
| one chain's composed parts | [`ComposedChain`] — index, its history reader, its balance fold, `(tag, factory)*`, declared units, poll interval | each chain's `Node::compose` |
| a verified chain, with its crate erased | [`Connection`] | each chain crate, for its own `Node` |
| the whole running system | [`Composition`] — `Index`, one `indexing::History` and one `indexing::Balances` handle routed per network, [`Wallets`], `base::Units`, `Schedules` | this crate |
| what a chain's index writes through | whatever that chain's own `Store` bound names, over `indexing::Repository` | `indexing::sqlite::Repository`, one value for every network |
| observation, and a watched address | [`indexing::Watching::watch`] of any [`Holder`] — a wallet, or a bare [`Subject`] | `indexing` |
| background send work | [`SubmissionRegistrar`], [`SubmissionTask`] | `base`'s, re-exported here; the application supervises and chains hand tasks over |
| history and balance | `history.history(&*wallet, `[`Page`]`)` and `balance.balance(&*wallet)` on [`Composition`]'s own handles — a wallet is a [`Holder`], and each answer belongs to the network that owns it, not to the index | each chain's `Node::compose`, folded by [`Composition`] |
| what an amount of an asset is | `base::Units` keyed by `base::AssetId`, and `base::Unit::display`, the one conversion out of the index's atomic units | each chain's `Config`, folded by [`Composition`]. No wallet carries it |

## Composition

Two phases, never one: **every chain's node identity is verified before any
storage is opened**, so a deployment pointed at the wrong node leaves no
database behind. The vector of [`Connection`]s cannot exist before the first
phase has finished, which is what makes the order a fact rather than a
convention.

```text
// PHASE 1 — identity only. The chain crates are named here and nowhere below.
let chains = vec![
    Box::new(bitcoin.connect(&secrets).await?) as Box<dyn Connection<Asset, Repo>>,
    Box::new(ethereum.connect(&secrets).await?),
];

// PHASE 2 — open the store, compose every chain over it, fold once.
let store = my_storage.open().await?;
let Composition { index, history, balance, wallets, units, schedules } = Composition::build(
    chains,
    store.repository(),
    &registrar,
).await?;
//  index:     Arc<indexing::Index>     — every chain, one Indexing
//  history:   Arc<dyn indexing::History>  — one reader per network, behind one handle
//  balance:   Arc<dyn indexing::Balances> — one fold per network, behind one handle
//  wallets:   wallets::Wallets<Asset>  — every factory under its tag
//  units:     base::Units              — every declared asset's unit, by identity
//  schedules: indexing::Schedules      — each chain's own cadence, for the Supervisor
```

[`Composition::verify_then_build`] is the same thing for a caller who would
rather state the ordering than remember it: it takes the verification as a
future and the store as a closure, and never calls the closure if any identity
check fails or the caller drops the future while they are running. That is what
`apps/server/src/chains.rs` uses; `examples/all_chains/main.rs` uses
[`Composition::build`]. There is no dependency aggregate: `registrar` is the
only thing composition needs that a chain cannot build for itself, and it is
passed once.

## Lifecycle

| Step | Call | Notes |
|---|---|---|
| 1. compose | [`Composition::build`], or [`Wallets::new`]`()` for a map assembled by hand | a factory map takes no index and no storage; nothing in one reads either |
| 2. add | [`Wallets::add`]`(asset, factory)` | one factory per asset tag; reads network, asset identity, and the asset's [`Authorship`] — `None` for one the system only receives — once. [`Composition`] does this for every chain's factories |
| 3. make | [`Wallets::create`], [`Wallets::from`], [`Wallets::secret`] | each returns and keeps nothing |
| 4. observe | `index.watch(id, &*wallet, start)` | the index's durable row commits **before** the caller files the key, so a crash leaves a watch-only address rather than a key nothing observes (`docs/design/030 §8 (ex-029)` section 3.3) |
| 5. sync | `indexing::Supervisor::run` | no selection argument: each index reads its own watch list on every pass |
| 6. use | [`Wallets::assets`], [`Wallets::asset`], [`Wallets::parse`], [`Wallets::accepts`], [`Wallets::send_all`], [`Wallets::author`] and then the [`Author`]'s own [`SingleSender::send`] and [`TransactionFactory`], and the index's `balance`/`history` of the wallet itself | [`Wallets::parse`] only produces addresses of that asset's network, which is why no send needs a separate same-network check. [`Wallets::author`] and [`Wallets::send_all`] refuse an asset the system never sends, at this lookup and nowhere else |

## Public API map

| Group | Items |
|---|---|
| compose | [`Composition::build`], [`Composition::verify_then_build`], [`ComposedChain`], [`Connection`] |
| construct | [`Wallets::new`], [`Wallets::add`], [`Wallets::secret`], [`Origin::new`], [`Dependency::of`] |
| use | [`Wallets`] methods above, [`Wallets::author`] for an asset this system spends, [`AssetInfo`]`::id` with `base::Units::of`, [`SendError::check_count`], [`SendError::definite`] |
| implement (chains) | [`WalletFactory`] (returning [`Generation`]), [`AddressParser`], [`FactoryProfile`], [`Holder`], [`SingleSender`], [`TransactionFactory`], [`Authorship`] (a [`Sender`] that also hands out an [`Author`]), [`Connection`] for the crate's own verified node |
| implement (application) | [`SubmissionRegistrar`], so background send work is supervised and drained on shutdown |
| async | [`FutureResult`] (a `base::BoxFuture` of this crate's [`WalletError`]), [`SendFuture`] |
| errors | [`WalletError`], [`WalletErrorKind`], [`SendError`] (accepted prefix, failed index, ambiguous id) |
| constants | [`MAX_TRANSFERS`] (50 per batch) |

## Usage

The same code works for every chain; only the factory passed in differs.

```rust
use std::{collections::BTreeMap, sync::Arc};

use base::{BlockPosition, Units};
use indexing::{Balances, History, Indexing, Start, Watching};
use wallets::{
    Holder, Page, SecretBytes, TransactionFactory, Transfer, Wallet, WalletFactory, Wallets,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

async fn run<F, X>(
    factory: F,
    index: Arc<X>,
    history: Arc<dyn History>,
    balance: Arc<dyn Balances>,
    units: Units,
    key: Vec<u8>,
) -> Result<()>
where
    F: WalletFactory + 'static,
    X: Indexing + 'static,
{
    let mut wallets = Wallets::<&str>::new();
    wallets.add("coin", factory)?;
    // The map answers what the asset *is*; `units`, which composition folded
    // from the chains' own configurations, answers what an amount of it is.
    let unit = units.of(&wallets.asset(&"coin")?.id).ok_or("the coin's unit")?;

    // The map makes wallets and keeps none; the id -> wallet map is the
    // caller's, and so is where the key lives.
    let mut held: BTreeMap<&str, Arc<dyn Wallet>> = BTreeMap::new();
    let treasury = wallets.from(&"coin", SecretBytes::new(key)).await?;
    let alice = wallets.create(&"coin").await?;

    // The index owns observation. Its row commits first, then the key is kept:
    // a crash between the two leaves a watch-only address, never a key nothing
    // observes. A known key states its own birthday; a new one is `Fresh`.
    index.watch("treasury", treasury.as_ref(), Start::At(BlockPosition(0))).await?;
    held.insert("treasury", treasury.clone());
    index.watch("alice", alice.as_ref(), Start::Fresh).await?;
    held.insert("alice", alice.clone());
    index.sync().await?;

    // Spending is the map's to grant: `author` refuses an asset this system
    // only receives, and nothing below it has to check again. A batch goes
    // through the asset's one `Sender`, which proves every wallet is that
    // factory's.
    let to = wallets.parse(&"coin", &alice.address().value)?;
    let treasury = wallets.author(&"coin", &treasury)?;
    let id = treasury.send(to.clone(), "0.5".parse()?).await?;
    let ids = wallets
        .send_all(
            &"coin",
            vec![Transfer { wallet: treasury.clone(), to: to.clone(), amount: "0.1".parse()? }],
        )
        .await?;

    // The same author builds, prices and broadcasts its own transaction,
    // with no chain crate named anywhere above.
    let mut builder = treasury.transaction();
    builder.transfer(to, "0.25".parse()?)?;
    let draft = builder.draft()?;                    // intent, before any node
    let signed = builder.prepare().await?;           // still local
    println!("{} costs {}", signed.id(), signed.fee());
    treasury.broadcaster().broadcast(&signed).await?;

    // Both reads are about the wallet itself, in exact atomic units; this
    // asset's own unit turns one into display units. Neither is the index's:
    // each is the repository's own query joined to one network's overlay, and
    // `Composition` hands one handle back per query, routed per network.
    let page = history.history(alice.as_ref(), Page::first(50)).await?;
    let total = balance.balance(alice.as_ref()).await?;
    let settled = unit.display(&total.amount)?;
    println!("{id} {ids:?} {settled} {} {}", page.transactions.len(), draft.transfers.len());
    Ok(())
}
```

Runnable versions with real chains: `examples/wallet/main.rs` and
`examples/all_chains/main.rs` ([examples/README.md](../../examples/README.md)).

## Related crates

| Layer (`linter.toml`) | Crates | Relation |
|---|---|---|
| packages | `crypto` | [`SecretBytes`] is re-exported from it, and [`SecretBytes::from_hex`] is the one secret-text parse |
| base | `base` | addresses, amounts, transaction ids, `Signer`, `TransactionBuilder`, `Draft`, `Broadcaster`, and [`SubmissionRegistrar`], which this crate re-exports |
| indexing | `indexing` | [`Holder`], [`Subject`] and [`Page`] (re-exported here), `Watching`, `Checkpoint`, the `History` readers and `Balances` folds [`Composition`] collects one of each per network, and the `Index`, `Schedules` and store ports it folds |
| chains | `chain-bitcoin`, `chain-ethereum`, `chain-solana` | implement [`WalletFactory`] and, for every asset they spend, [`Authorship`] |
| api, server | `payment-api`, `payment-server` | `payment-server` calls [`Composition::verify_then_build`] and holds the [`Wallets`] it returns; `payment-api` serves it over HTTP and keeps the id -> wallet map |

Design notes: [`ARCHITECTURE.md`](../../ARCHITECTURE.md) (Wallets, Transaction
flow), [`docs/INDEXING.md`](../../docs/INDEXING.md) (initial progress and
birthdays), [`docs/API.md`](../../docs/API.md).

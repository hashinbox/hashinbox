# Chain crate layout

Every crate under `sdk/chains/<chain>` has the same files, the same nouns, and
the same construction sequence. `sdk/chains/ethereum` is the worked example: to
find something in another chain, find it here first, then open the file with
the same path there. `linter.toml` enforces the tree and
`scripts/check-chains.sh` the `native/` boundary.

## 1. Tree

Required paths are exact. `native/` below its two required files is the
chain's own shape; ethereum's is the recommended one.

```text
sdk/chains/ethereum/
  Cargo.toml                  chain-ethereum; depends on packages, base, indexing, wallets only
  README.md                   crate docs (the rustdoc root): contracts table, model, init, native vs adapter
  tests/
    address_conversion.rs     native Address <-> base::Address (canonical, strict inbound)
    transaction_adapter.rs    an external Preflight drives the native send pipeline
    vocabulary.rs             RpcClient::connect -> block() -> parse() -> transactions -> movements
  src/
    lib.rs                    #![doc = README], `pub mod native`, re-exports only
    config.rs                 Config<A>: this chain's section, public fields, new/network/validate/
                              connect; Node<A>: the verified node, network/compose, and its
                              wallets::Connection impl; Token<A> on a chain with a second asset
    error.rs                  From<native::Error> for SourceError, IndexError, TransactionError, wallets::WalletError;
                              submission/ambiguous/definite: the only error-kind mapping, plus
                              the two inbound conversions composing needs: From<config::Error>
                              and From<IndexError> for native::Error
    identity.rs               every conversion to and from a base identity (section 3, Address):
                              base::Networked for Network, Address::canonical(&impl Networked),
                              strict TryFrom<&base::Address> for Address, TransactionId::canonical(),
                              From<TransactionId> for base::TransactionId, AssetKind::id()
    source.rs                 Source<C>: ChainTip + BlockSource; IndexedBlock for RawBlock
    interpreter.rs            BlockInterpreter: Block facts -> ObservationDraft, plus settlement():
                              whether this chain's fee is already inside those movements
    indexer.rs                Indexer<C, R> = indexing::Service<Source<C>, BlockInterpreter, R>
                              (a type alias, not a newtype) + IndexerParams + indexer(params)
    key.rs                    Signer for native::KeyPair
    wallet.rs                 WalletConfig, which is also this chain's wallets::AddressParser:
                              the only lenient address parse, store-free and I/O-free, so a
                              caller holding a Network validates address text in plain code;
                              WalletFactory (wallets::WalletFactory, AddressParser delegating
                              one line to the config, FactoryProfile: network, asset
                              identity, and the asset's Authorship — None for one this system only
                              receives); the wallet (Holder, Signer = wallets::Wallet, plus
                              SingleSender and TransactionFactory = wallets::Author), so no caller
                              downcasts; ethereum also BalanceWitness (section 6), the only chain
                              with one
    builder.rs                TransferBuilder: base::TransactionBuilder (transfer, draft, prepare)
                              + its snapshot encoding, which draft() carries
    sender.rs                 Batch: wallets::Sender -> Outbox::send
    sender/                   the send pipeline (section 7): outbox, reservation, dispatch, recovery,
                              stub (#[cfg(test)] node double)
    native/
      mod.rs                  protocol surface and re-exports
      error.rs                Error { kind, message, retryable }, ErrorKind
      network.rs              CHAIN_NAME (the one chain key), NETWORKS, Network
      amount.rs               Wei
      asset.rs                ETH metadata, AssetKind
      address.rs              Address (parse, display)
      key.rs                  KeyPair (generate, import, derive address): wraps crypto::KeyPair
      erc20.rs                ABI calls, TRANSFER_TOPIC, Log -> Movement
      rpc/                    Client (connect, network), Accounts, block reads, codecs;
                              submitter.rs: Submitter { broadcast, known } -> native::Error;
                              witness.rs: Witness { attest } -> Attestation (chain-only; the
                              only `Witness` in the tree — solana's batch account read is a
                              `native::Bracket`, section 6)
      block/                  RawBlock, Block, Transactions, Transaction, Receipt, Log, Trace,
                              Balance, Movement, Fee, Withdrawal (chain-only)
      transaction/            Transfer, BuildContext, Builder, Unsigned/SignedTransaction,
                              TransactionId, FeeLimits; preflight.rs Preflight, funds.rs Funds,
                              nonce.rs Nonces (the chain's sequencing rule)
```

`sender/` is required on every chain (section 7). Allowed extensions, nothing
else: a directory `source/`, `interpreter/`, or `wallet/` holding submodules
of the same-named file when that file outgrows the 500-production-line limit
(section 5) or owns a chain-only
part of that contract, and chain-only files in `sender/` (bitcoin
`sender/utxos.rs`, the index-backed UTXO view the outbox reserves from; solana
`sender/registration.rs`, the supervised submission task). Chain-only protocol
files go in `native/` (bitcoin `native/prevout.rs`, solana `native/memo.rs`).
No `examples/`, `benches/`, or `build.rs` in a chain crate.

## 2. Native vs. adapter

| | `src/native/` | every other `src/` file |
|---|---|---|
| Contains | RPC, wire codecs, block/transaction parsing, ABI/script/instruction decoding, keys, amounts, fee/nonce/funds rules, build, sign, submit | one SDK contract per file: trait impls, native -> canonical mapping, error conversion, the send pipeline's process state |
| May use | primitive `base` inputs only (below), `json_rpc`, `crypto`, chain libraries, `crate::native::*` | `crate::native`, `indexing`, `wallets`, `base` |
| Must not | name `indexing` or `wallets`; `use crate::` outside `native`; implement an SDK trait outside tests; return or convert to an SDK type (`base::TransactionError`, `base::Network`, `base::Address`, `base::TransactionId`, `TransactionRef`) | call JSON-RPC, parse bytes, hard-code protocol constants |
| Errors | one `native::Error` with `retryable`; batch rules return `(index, native::Error)` | `error.rs` converts; nothing else maps errors ad hoc |

The primitive `base` inputs `native/` may name: block coordinates (`BlockRef`,
`BlockHash`, `BlockHeight`, `BlockPosition`, `BlockParent`), `Decimal`,
`BoxFuture`, the signing input (`Signer`, `SignRequest` and its payload,
scheme, encoding, and key-format values, `SignFuture`, `SignedPayload`,
`Signature`, `PublicKey`, `Curve`, `SignerError`), `transaction::Envelope`,
chain metadata (`ChainNetwork`, `ChainName`, `ChainNetworks`, `NetworkKind`,
`Asset`), and the block vocabulary (`Identified`, `Block`,
`Transactions`, `MovementKind`). Everything in that signing list except
`Signer` and `SignFuture` is defined by `crypto` and re-exported by `base`, so
either spelling is a primitive input.

`crypto` is a primitive package and stays unrestricted in `native/`, with one
placement rule: `crypto::KeyPair` is the local signing engine of every chain —
secp256k1 for Bitcoin and Ethereum, Ed25519 for Solana — so it belongs to
`native/key.rs` and tests only. `scripts/check-chains.sh` rejects anything
else.

## 3. Vocabulary

One meaning per noun, spelled the same in every chain.

| Noun | Meaning | Ethereum |
|---|---|---|
| `Network` | native network identity; it implements `base::Networked`, whose `canonical()` is its `base::Network` (what every index row is partitioned by) | `{ chain_id, slug }` |
| `RawBlock` | exact source evidence for one block, kept for replay; the only "raw" noun | block JSON + receipt JSON + the block's `prestateTracer` diff, held for one pass and never stored |
| `Block` | one parsed block | `base::Block<Transaction>` |
| `Transactions` | a block's transactions in order, unique ids | `base::Transactions<Transaction>` |
| `Transaction` | one parsed on-chain transaction: `id()`, `succeeded()`, `fee()`, `movements()` + chain slot | slot: `from`, `to`, `value`, `index`, `receipt` |
| `Address` | typed native address; `Display` is canonical text; `canonical(&impl Networked)` is its `base::Address` | 20 private bytes (`from_bytes`/`as_bytes`), lowercase `0x` hex |
| `TransactionId` | typed native id; `Display` is canonical text; `canonical(&impl Networked)` is its `TransactionRef` | Keccak-256 hash, 32 private bytes (`from_bytes`/`as_bytes`) |
| `Movement` | one value change in native types; the interpreter maps it to `ValueMovement` | ether value, ERC-20 `Transfer` logs |
| `Fee` | network fee of one transaction | `gas_used * effective_gas_price`, payer = sender |
| amount (`Satoshi`, `Wei`, `Lamport`) | one shape on every chain: private field, `from_atomic`/`units` (the chain's own atomic width), `atomic()` (that count as a `base::Decimal`, the form indexing stores), `from_decimal`/`decimal` (the chain's display unit), `checked_add`, `checked_sub` | `Wei([u8; 32])`; `decimal_with(decimals)` reads an ERC-20 amount in its token's decimals |
| `Transfer` | one **requested** payment, never an observed change; no sender field: the sender is whoever signs it. The recipient is `to` at every level — `native::Transfer`, `wallets::Transfer` and the chain-neutral `base::Transfer` a `Draft` carries. `destination` survives only on the HTTP wire, where `apps/api` names it, and `base::Payment` is retired | `{ asset, to, amount }` |
| `UnsignedTransaction` / `SignedTransaction` | authoring forms; signed = `{ id, envelope }` | EIP-1559 |
| `Envelope` | exact signed wire bytes | `base::transaction::Envelope` |
| `Source` | the indexer's block source, nothing else; only `src/source.rs` declares one (`scripts/check-chains.sh` check 9). It also implements `ChainTip`, where the chain states its own confirmed line: every chain must answer `confirmed_tip` in its own terms, and there is no default. A sender of a send is a `Payer` | `Source<C>` |
| `Submitter` | the RPC broadcast capability: `broadcast` the exact bytes, `known` the exact ID | `native::rpc::Submitter` |
| `Preflight` | the node checks one send needs before signing or submitting | `native::transaction::Preflight` |
| `Funds` | what a batch's senders must afford, a pure rule; it builds nothing. On a UTXO chain the answer names the outputs, because there is no balance to name instead, and the transaction that pays a group of them is `Builder::grouped` | `native::transaction::Funds` |
| `Outbox` | the send pipeline: reserve → prepare → sign → submit → settle | `sender::Outbox` (`pub(crate)`) |
| `Item` | one transfer with its sender and signer, as the outbox takes it | `sender::Item` |
| `Reservations` | the outbox's process-local state; `Admission` guards the reserved resource while it is prepared, `Settlement` while one submission is settled, and `PreparedTransaction` is one signed transaction with the resource it takes. `Claim` is **not** this vocabulary: `base::Claim` is the set of resource names a signed transaction reports, "a report, never a lock", and one noun cannot also be the live RAII guard (`docs/DEFECT_PATTERNS.md` section 2.3) | `sender::reservation` |
| `Dispatch` / `Recovery` | submit exact bytes and settle; settle an unknown outcome. Recovery *settles* on every chain: solana's seam is `Settler::settle`, and what its inspection concludes is a `Verdict` | `sender::dispatch`, `sender::recovery` |

Banned in chain crates: `Parsed*`, `*Data`, `Interpreted*`, `Facts`,
`Movements`, public `*Wire` types, `Blocks` (never a list type), a trait named
`Transactions`, an `Envelope` of its own, `IndexerSettings`, `<Chain>Address`
aliases, `type Source`, `Coordinator`, `Preparation`, `Funding`, a
`from`/`source` field on `Transfer`. `scripts/check-chains.sh` check 7 rejects
the type definitions — **including** the second list below, which it did not
until `struct Preparation` was found in bitcoin's outbox having sat past the
claim; the one name that cannot join it is `Context`, banned only for a
slot-stamped value, which no regex can tell from bitcoin's per-block parsing
`Context`. indexing's `InterpretedBlock` is a contract type the interpreter
returns, not a chain type.

Also banned, because each said nothing or said two things at once: a bare
`Entry` — name what is held, and for the signed transaction a reservation holds
there is exactly one name on every chain, `PreparedTransaction` (offering two
remedies forced no choice, and solana kept the unnamed `(SignedTransaction,
u64)` tuple the ban existed to prevent); `Attempt` (a bounded read is a `Pass`), `Evidence`
(a conclusion is a `Verdict`), `Resolver` (recovery *settles*, through a
`Settler`), `MemoSource` and any other `*Source` that is not the block source
(a sender of a send is a `Payer`), `UtxoSet` (the plural names the
collection: `Utxos`; the service that reads them is a `UtxoReader`), and
`Context` for a slot-stamped value (`rpc::Reading<T>`). `floor` is the
ethereum nonce floor only; a minimum context slot is `min_slot`.

A plural noun is a collection of values, never a trait: the indexing storage
ports are `WatchStore`, `IndexStore`, `BalanceStore` and `OutputStore`, so
`Transactions` always means `base::Transactions<T>`.

Banned everywhere in `sdk/`, `apps/`, `examples/` (`scripts/check-chains.sh`
rejects them): `CanonicalAddress`, `AddressText`, `AddressEncoding`,
`AddressFormat`, `AddressValidator`, and a `BaseAddress` alias (write
`base::Address`; a retired design note (011)); `HistoryReader`, `IndexedHistory`,
`selected_history`, `history_asset` (history is one repository query by asset —
`indexing::ConfirmedHistory`; the value that joins one such page to one index's
overlay is `indexing::ObservedHistory`, and no chain declares another;
docs/design/030 §8 (ex-012)); `Provider`, `WalletProvider`, `Family`, `enroll`,
`enroll_with_registry` (one factory concept, `WalletFactory`, added once with
`Wallets::add`; a retired design note (014)); `IndexScope`, `TestnetMap`,
`TransactionEnvelope`, `MemoSource`, `UtxoSet`, `Holding` (one network
vocabulary and one meaning per noun; a retired design note (018)); `ScopeState`,
`ScopeFailure`, `ScopeHealth`, `ScopeEvent`, `ScopeAdmission`,
`ScopeMismatch`, `scope_changed` (the `(chain, network)` partition is a
**network**, never a "scope": `NetworkState`, `NetworkFailure`,
`NetworkHealth`, `NetworkEvent`, `IndexErrorKind::NetworkMismatch`,
`SyncMonitor::network_changed`).

`Holding` stays banned, and `docs/design/030 §8 (ex-029)` does not bring it back: the value
it describes — one address and the asset it is read in, with no wallet behind
it — is `indexing::Subject`, the bare counterpart of `indexing::Holder`, which
a `wallets::Wallet` already is. "Holding" reads as a balance position, which
is the second meaning the ban exists to prevent.

The ban covers the lower-case spelling, and it took a second check to say so.
The list above is matched case-sensitively, which is right for a type name —
`Holding` in prose is the argument for the ban, `Holding` in code is the
regression — and that left `mod holdings;`, `fn holding(..) -> Subject` and
`let holding = Subject {..}` sitting in the tree. Check 6b of
`scripts/check-chains.sh` is the same list again, case-insensitively, for the
retired nouns that are also ordinary English words (`holding`, `provider`,
`family`), matched only outside comments and string literals so prose stays
free to name what it is explaining, and on whole words so `holding` is caught
where `without_holding_the_lock` is not.

### Addresses

One address outside a chain crate: `base::Address { network, value }`, where
`value` is the chain's canonical text, so string equality is address
equality. Index rows, registry rows, filters, wallets, sends, and the API all
carry it. Inside a chain crate only the typed native `Address` is used. The
two meet in `identity.rs` — both conversions between the two types — and in
`wallet.rs`, which owns the one lenient parse of user text:

| Direction | Where | Rule |
|---|---|---|
| native → `base::Address` | `Address::canonical(&impl base::Networked)` | the canonical `Display` text in that network; the argument is the chain's own `Network` or the `base::Network` itself |
| `base::Address` → native | `TryFrom<&base::Address> for Address` | strict: this chain, a known network (bitcoin), exact canonical text; for anything read from storage, filters, or another wallet |
| user text → `base::Address` | `WalletConfig` as `wallets::AddressParser` (`WalletFactory`'s own impl, which its `wallets::WalletFactory` supertrait bound requires, delegates one line to it) | lenient: every textual form the chain defines, in the configuration's network (ethereum: any case, EIP-55 checked when mixed; bitcoin: any case, network-checked; solana: Base58 only), returned canonical |

A wallet computes its `base::Address` once when it is attached and returns
it from `wallets::Addresser::address`.

**The lenient parse belongs to the config, not the factory, on every chain.**
Address text is pure — canonical form, network membership, checksum, encoding —
and none of it needs a store, a node, or an index. It sat on `WalletFactory`
only because that is where the supertrait bound put it, and two of the three
`WalletFactory::new` signatures take *consumer* capabilities (section 6:
bitcoin one `OutputStore` reader, solana an `Arc<Indexer>` bounded on
`Checkpoint + History`), so a third party embedding this SDK over its own
repository could not reach the parse at all and fell back to
`base::Address::new` and its own trust. `WalletConfig::new(&Network)` is
constructible in plain code, so the config is the home. The factory keeps its
impl — the bound requires one — and that impl does nothing but delegate, which
is the whole point: **one parse, not two that agree.**
`scripts/check-chains.sh` check 10 is that claim mechanically — `impl
AddressParser for WalletConfig` on every chain, and the factory's body being
exactly `self.config.parse_address(text)`. Each chain's
`tests/address_conversion.rs` proves reachability from outside the crate
(`user_text_validates_from_a_network_alone`, using only `Network` and
`WalletConfig`), and each `wallet.rs` proves the agreement
(`the_factorys_parse_is_the_configurations_own`, over accepted *and* refused
text, against a config built from a `Network` alone).

**What "valid" proves is not the same amount on each chain**, and the doc
comment on each impl says so rather than implying one standard:

| Chain | Accepting the text proves | It does not prove |
|---|---|---|
| bitcoin | a Base58Check or Bech32/Bech32m encoding with a correct checksum, whose version byte or witness HRP names *this* network (testnet, signet and testnet4 share prefixes, so the network comes from the config), and whose script kind is one this crate can pay | that the address exists, is funded, or has a key behind it |
| ethereum | exactly twenty hexadecimal bytes behind `0x`, plus the EIP-55 checksum **only when the text is mixed case** — the weakest of the three by construction | anything about single-case text beyond its length: a typo in it is a different, equally valid address. The network is not in the text at all (every EVM chain shares one encoding), so it comes from the config |
| solana | canonical Base58 — re-rendering the bytes returns the same string — of exactly thirty-two bytes | that the bytes are an Ed25519 point. An off-curve (program-derived) address parses; whether native SOL may be sent to one is a transfer rule on the send path (`docs/adr/0006`), not an address rule. The cluster is not in the text either |

## 4. Construction

The same lines on every chain; only the `use` line and `Network` differ.

```rust,ignore
use chain_ethereum::{IndexerParams, KeyPair, Network, RpcClient, RpcConfig, WalletConfig, WalletFactory, indexer};

let network = Network::new(31_337, "anvil");
let transport = json_rpc::Endpoint::new(url).connect(secrets)?;
let rpc = RpcClient::connect(transport, &network).await?; // verifies identity
let repository = open_store()?;                                                    // one store, every network
let indexer = Arc::new(indexer(IndexerParams::new(rpc.clone(), repository))?);                     // sync, no I/O
let factory = WalletFactory::new(WalletConfig::new(&network), &rpc)?;                                // sync, no I/O
let wallet = factory.attach(KeyPair::generate()?)?;                                                // optional local key
let mut wallets = wallets::Wallets::new();                                                          // factories only
wallets.add("eth", factory)?;                                                                      // one factory per asset
```

Rules behind it:

- `RpcClient::connect(transport, &Network)` is the only public client
  constructor and verifies chain identity; `rpc.network()` returns it.
- Every constructor that needs the SDK's identity takes `&impl base::Networked`
  and converts internally, so a caller names only its chain's own `Network`.
  `base::Networked::canonical` is that conversion; nothing else computes one.
- `IndexerParams { rpc, repository, sync }` has public fields;
  `IndexerParams::new` fills `IndexerParams::SYNC`. Override with struct update
  syntax, then hand the value to the chain's `indexer(params)` constructor.
  `Indexer<C, R>` is a **type alias** for
  `indexing::Service<Source<C>, BlockInterpreter, R>`: the newtype that used to
  wrap one, and the `indexing::delegate_indexer!` macro that forwarded every
  capability to its field `0`, were deleted — a `Service` already answers the
  whole `indexing::Indexing` surface, and hands out the query readers over its
  own store rather than implementing one.
- `WalletConfig::new(&Network)` has documented defaults; `with_*` overrides one
  choice each. `WalletFactory::new(config, &rpc[, index reads]) -> Result`
  builds its own RPC views and its `Outbox`, and takes only the index reads
  the chain's wallets use (section 6): ethereum none, bitcoin one
  `Arc<dyn indexing::OutputStore>` — `Indexer::outputs()`, the reader over the
  store, for coin selection — solana `&Arc<Indexer>`, on which it registers its
  own commit listener for recovery. It
  implements `wallets::{WalletFactory, AddressParser, FactoryProfile}`: the
  chain owns address text, so no caller knows an address encoding. Its
  `AddressParser` is one delegating line, because that parse is
  `WalletConfig`'s (section 3, Addresses) and needs none of the dependencies
  above — a caller with a `Network` and a string validates text without ever
  building a factory. Key text is
  *not* a chain contract: all three import formats are 32 bytes as 64
  hexadecimal characters, so callers decode with `crypto::SecretBytes::from_hex`
  and `WalletFactory::create` checks the length and the curve
  (`KeyPair::import`, over `crypto::KeyPair::secp256k1` or
  `crypto::KeyPair::ed25519`: the curve is the only thing a chain chooses).
  `WalletFactory::generate` returns a `wallets::Generation { wallet, material }`;
  `Wallets` persists the material in the registry, and `Wallets::secret(tag)`
  returns it to a caller that wants a key before a wallet.
- Neither a history page nor a balance is a chain contract, and neither is the
  index's either. The
  repository answers its own `indexing::ConfirmedHistory::history(HistoryQuery)`,
  `indexing::ObservedHistory` joins that to the chain's overlay to answer
  `indexing::History::history(&dyn Holder, Page)` in atomic units, and
  `apps/api` scales it. A balance takes the same route: the repository's own
  total, completed by the same overlay. A chain builds one reader and one fold
  for its own network in `compose` and hands both back on
  `wallets::ComposedChain`, and a consumer holds each as a value rather than
  reaching for it through an indexer.
  A factory states only which asset its wallets hold
  (`FactoryProfile::asset`); what an amount of it *is* belongs to the asset and
  is declared by `Config::units() -> base::Units` — its own currency from the
  chain's `native::asset` constant, and each allowlisted token in the scale
  `connect` checked its contract against. `wallets::Composition` folds every
  chain's `Units` into one map keyed by `base::AssetId`, and refuses to compose
  a factory whose chain declared no unit for its asset.
- `KeyPair::generate()` / `KeyPair::import(secret)` / `KeyPair::generate_secret()`
  take no network.

### Plugging into the composition root

A chain composes itself, and `wallets::Composition` holds any number of them.
`src/config.rs` holds the chain's whole half of it, and the three chains are
structurally identical — same file, same type names, same function names, same
argument order (`docs/design/030` section 5.1 steps 9 and 10). What each one
hands back is `wallets::ComposedChain<A>`, declared once in `wallets` rather
than three times here:

```rust,ignore
// sdk/chains/<chain>/src/config.rs

// What a store must answer for THIS chain to compose over it, named here
// because it is this chain's requirement and not a rule about stores.
// `indexing::Repository` (WatchStore + IndexStore) is the minimum any index
// needs; the rest is what this chain's own consumers read. Bitcoin adds
// `OutputStore` for coin selection; ethereum and solana do not.
pub trait Store:
    indexing::Repository + indexing::BalanceStore + indexing::ConfirmedHistory
    + indexing::OutputStore                 // bitcoin only
    + Clone + Send + Sync + 'static {}
impl<R> Store for R where R: … {}           // blanket, so nothing declares it

pub struct Config<A> { /* public: network, rpc, asset, this chain's own keys, cadence */ }

impl<A: Ord> Config<A> {
    pub fn new(network: Network, rpc: sdk_config::Rpc, asset: A) -> Self; // everything else defaults
    pub fn network(&self) -> base::Network;                           // no node, no storage
    pub fn validate(&self) -> Result<(), Error>;                      // the value alone
    pub async fn connect(self, secrets: &dyn json_rpc::Secrets) -> Result<Node<A>, Error>;
}

impl<A> Node<A> {                                          // PHASE 1 done: identity verified
    pub fn network(&self) -> base::Network;                // what it verified against
    pub async fn compose<R: Store>(
        self, repository: R, registrar: &Arc<dyn SubmissionRegistrar>,
    ) -> Result<wallets::ComposedChain<A>, Error>;         // PHASE 2: index + factories
}

// The same call with this chain erased, so one vector holds every chain.
// `Connection` names no network: one repository serves every one of them.
impl<A: Send + 'static, R: Store> wallets::Connection<A, R> for Node<A> { … }

// Registration: how a document's section named after this chain reaches it.
// `sdk/config` names no chain crate, so the chain hands itself in.
impl<A: Ord + Send + Sync + 'static, R: Store> sdk_config::Chain<A, R> for Config<A> {
    fn network(&self) -> base::Network;                            // no node, no storage
    fn accept(&self) -> Result<Option<String>, sdk_config::Error>; // validate + what to tell an operator
    fn connect<'a>(self: Box<Self>, secrets: &'a dyn json_rpc::Secrets)
        -> base::BoxFuture<'a, Result<Box<dyn wallets::Connection<A, R>>, sdk_config::Error>>;
}

// sdk/wallets/src/composition.rs — declared once, returned by every chain
pub struct ComposedChain<A> {
    pub index: Arc<dyn indexing::Indexing>,
    pub history: Arc<dyn indexing::History>,   // this network's page reader
    pub balance: Arc<dyn indexing::Balances>,  // this network's balance fold
    pub factories: Vec<(A, Box<dyn wallets::WalletFactory>)>,
    pub units: base::Units,
    pub poll: Duration,
}
```

An application never writes a `Connection` impl and never folds the results by
hand: it collects the verified nodes and hands them to `wallets::Composition`,
which returns one `indexing::Index`, one `wallets::Wallets<A>` and one
`indexing::Schedules` (`sdk/wallets/README.md` §Composition).

Rules behind it:

- **The config type is the API.** Every field is public, `Config::new` fills
  the chain's own defaults, and `serde`'s `default = …` attributes call the
  same functions, so a value built in code and a value read from a document
  cannot drift. An embedder needs neither a file nor `sdk/config`'s `toml`
  feature to build one; `Deserialize` is derived unconditionally, because
  every chain links `serde` to talk to its node anyway. The registration hook
  is sugar layered on top of the value, never the only door.
- **`A` is the asset tag**, never `payment_api::AssetTag`: `linter.toml` gives
  a chain crate no `api` layer. The chain stores tags and never interprets
  them. An absent `asset` key is the chain's own text (`btc`, `eth`, `sol`)
  through `A: FromStr`.
- **`validate` owns values, the deserializer owns encodings.** Anything an
  in-code builder can get wrong — a zero token contract, a zero bound, a
  repeated tag — is checked in `validate`; only text checks (canonical address
  encoding, Base58 genesis) stay in `deserialize_with`. `connect` calls
  `validate` first, so reaching a node without the check is impossible.
- **Each chain names its own store requirement.** `Store` is declared in this
  crate's `config.rs` rather than in `wallets`, because it is this chain's
  requirement and not a rule about stores: `indexing::Repository` — the watch
  list and the commit — is the minimum any index needs, and everything above it
  is what this chain's own consumers read. An account chain needs no
  `OutputStore`, and a third party's sink that answers none of the reads still
  indexes.
- **Two phases, never one.** Every identity check must finish before any
  storage is opened, so `connect` (nothing opened, nothing written) and
  `compose` (over an already-open repository) cannot be one call. A node that
  fails `connect` fails the deployment: the application verifies every chain
  before it opens anything, so that is a refusal to start rather than a
  per-chain quarantine — quarantine is an indexing failure, after startup.
- **`registrar` is an argument on every chain**, so an application passes it
  uniformly; only Solana uses it (`WalletFactory::with_registrar`), because
  Bitcoin and Ethereum finish every send before the call returns.
- **A library writes nothing to standard output.** Bitcoin's
  confirmation-depth banner is *stated* by the chain, as the `Some(..)` of its
  `Chain::accept`, and *printed* by the application. The server no longer
  names Bitcoin to print it.

What is left in `apps/server` is **one line per chain**:
`registry.register::<chain_<name>::Config<AssetTag>>("<name>")`. Nothing else:
the chain crate carries its own `Connection` and `sdk_config::Chain` impls,
`sdk/config` routes the section, and `wallets::Composition` does the folding.
`main`, the routes, the wire
models, and the OpenAPI contract take a new asset tag as data
(`GET /v1/assets`). `apps/server/src/chains.rs` is the only module that names a
chain crate (`scripts/check-chains.sh` check 8; `linter.toml` keeps `apps/api`
free of chain dependencies).

## 5. Naming rules

- The module path carries context: `native::transaction::Builder`, not
  `TransactionBuilder`; `native::rpc::Client`, re-exported at the root only as
  `RpcClient` (and `rpc::Config` as `RpcConfig`), the one allowed rename.
- Adapter types never reuse a `base`/`wallets` trait name: `TransferBuilder`
  implements `base::TransactionBuilder`.
- One crate error: `Error`/`ErrorKind` (= `native::Error`). No `ChainError`,
  no per-module error enums.
- **Error classification carries its subject where two of them can meet.** The
  bare pair `Error`/`ErrorKind` belongs to a crate or module that owns the
  namespace a reader is already in: a chain's `native::`, `json_rpc::`,
  `crypto::`, `http_server::`, `sdk_config::`. The two SDK capability crates
  are named, because their errors are imported *into* files that already have
  an `Error`: `indexing::IndexError`/`IndexErrorKind` and
  `wallets::WalletError`/`WalletErrorKind`. A second classification inside one
  crate is named too (`crypto::SignerErrorKind`,
  `http_server::ConfigErrorKind`, `base::TransactionErrorKind`). The
  checkable consequence: **no file imports a workspace error type under an
  alias.** What is left is `std::error::Error as StdError` (std's trait beside
  a crate's struct), jsonrpsee's `Error as RpcError` in
  `packages/json-rpc/src/http.rs` (a dependency's name, which no rename here
  reaches), and `base`'s own re-exports in `sdk/base/src/lib.rs`, which give
  the `transaction` module's short names the module's noun at the crate root.
  `kind` stays a public field — see `docs/design/017` `U7`.
- Ids, addresses, and amounts are typed (`TransactionId`, `Address`, `Wei`),
  never raw bytes, strings, or `U256`.
- Every public item is documented. The lint is workspace-wide
  (`missing_docs = "deny"` under `[workspace.lints.rust]` in the root
  `Cargo.toml`), so no `lib.rs` repeats it.
- One chain key per chain: `CHAIN_NAME: base::ChainName`. `base::ChainNetwork`
  metadata (`NETWORKS`, the assets' `network`) names it in
  `ChainNetwork::name`; there is no `&str` twin. The plural is the chain's
  *networks*, never its chains: one `ChainNetwork` is one network, and
  `NETWORKS: base::ChainNetworks` is the mainnet plus the named test networks
  of this one family.
- A file is limited to **500 production lines**. That is exactly what
  `linter.toml`'s `rust/file-length` counts and what `AGENTS.md` states: lines
  inside test-only items and modules, and standalone test files, do not count.
  The limit bounds how much behaviour one file owns; a file's own test module
  is evidence about that behaviour, not more of it, so a long `#[cfg(test)]`
  block is not a reason to split (`solana/src/sender/outbox.rs` and
  `ethereum/src/sender/reservation.rs` are the worked examples, closed as
  `docs/design/017` `U4`). Any stricter "including tests" reading is retired.

## 6. Where a chain differs

Only these differences are allowed; each lives in the file named. Anything not
in this table is drift, not a decision. Work that was consciously *not* done,
and the asymmetries that outlived this table's last review, are recorded in
`docs/design/017-remaining-work.md`.

This table, the required tree in `linter.toml`, `scripts/check-chains.sh` and
`examples/tests/scenarios.rs` are how chain shape is enforced. A bundled `Chain` / `ChainIndexer` / `ChainFamily` trait
with associated `Network`/`Source`/`Interpreter`/`KeyPair` types is **not**,
and will not be: no generic function consumes every one of those types at
once, `WalletFactory::new` takes different dependencies per chain by design
(this table's own row), and the tooling above catches file names, module
boundaries and banned nouns that no trait can. The one piece of it worth
having shipped on its own: `base::Networked`, the single-method trait every
native `Network` implements. `a retired design note (018).md` section 8 is the
reasoning; `docs/design/017` `U1` is closed against it.

| Slot | Rule | Bitcoin | Solana |
|---|---|---|---|
| `Network` | a type with `slug()` implementing `base::Networked`; constructed however the chain needs | closed enum: `Network::Regtest` | `Network::new(slug, GenesisHash)`; `connect` also requires the executable Memo-v3 program |
| `Config` section | `config.rs`, section 4: the same types, the same functions and **the same section shape** on every chain — its own keys, one `rpc`, one optional nested `cadence`; only the chain's own keys differ. The flattened-versus-nested asymmetry this row used to record was resolved by `docs/design/030` step 11, when each chain became the parser of its own section | adds `confirmation_depth` — the safety number with no node answer behind it, stated back through `Chain::accept`, the only chain with a notice — plus `address_type` and `limits` (`fee_target_blocks`, `max_fee_rate`), which is how a deployment reaches `WalletConfig::with_address_type`, `with_fee_target_blocks` and `with_max_fee_rate` at all; before those keys existed, the fee ceiling `WalletConfig::validate` checks was one no operator could set | adds `genesis_hash` (configured and verified, never discovered) and `tokens` |
| `KeyPair` | never takes a network | names no address: `factory.attach` derives it from `WalletConfig`'s network and `AddressType`, so `key.rs` holds only `Signer` | names its address (a cluster-free Ed25519 key); `key.rs` holds only `Signer` |
| `WalletFactory::new` | `(config, &rpc)` plus only the index reads its wallets use; ethereum takes none | `(config, &rpc, spendable: Arc<dyn indexing::OutputStore>)`: coin selection, the pending overlay folded in so change is spendable at depth one, served by `Indexer::outputs()` — an `indexing::ObservedOutputs` over the store, not the index itself, because the rows are the store's. **One read, not two.** This argument was a pair twice over — an `Outputs { spendable, confirmed }` struct (a plural naming a fixed set of roles, `docs/DEFECT_PATTERNS.md` section 2.5), then two named arguments — and the `confirmed` half was deleted with `Indexer::confirmed` because no wallet ever read it: a Bitcoin wallet reports no balance, and every balance is `indexing::Balances` over the same rows (`docs/DEFECT_PATTERNS.md` sections 1.1 and 3.1). Coin selection is the only output read a wallet performs, so it is the only one the factory takes | `(config, &rpc, &Arc<Indexer>)`: checkpoint, commit announcements, and the sender's SOL history, for recovery |
| `BlockInterpreter::settlement` | every chain states where its fee already sits, and there is no default: a balance fold that guessed would be wrong by exactly one fee per transaction | `FeeSettlement::Movements`: the fee *is* `Σ inputs − Σ outputs` | `FeeSettlement::Charged`, like ethereum: the fee is outside the transfer |
| Block-level credit | a value change the block makes that no transaction names, carried as `indexing::InterpretedBlock::credits` and folded by `indexing::Ledger::apply_credit`. Only ethereum has one: post-Shanghai consensus-layer withdrawals, read from the `eth_getBlockByNumber` payload already fetched by `native::block::RawBlock::withdrawals` (Gwei converted to wei at the parse) and turned into a `BlockCredit` by `interpreter.rs`, which selects the watched beneficiaries exactly as it selects watched transactions. No extra RPC call, and the credit is deliberately not a transaction: it has no `TransactionRef`, so it appears in a balance and never in a transaction page (`docs/design/021` section 4.4) | **none, confirmed**: every satoshi arrives as an output, coinbase included — `credits` is always empty | **none that this SDK manages**: block `rewards` credit stake and vote accounts, and the source does not fetch them (`"rewards": false`) — `credits` is always empty |
| Balance witness | an independent read that can vouch for a folded balance at the block it settled at, and never replaces it. Only ethereum has an oracle to pin: `eth_getBalance` and ERC-20 `balanceOf` at `{blockHash, requireCanonical}` are deterministic EVM execution, so `WalletFactory::witness` -> `BalanceWitness::attest` is the one thing that promotes a fold to `indexing::Completeness::Complete` | **absent**: Core keeps no address index, and this crate uses no `scantxoutset` and no wallet RPC | **absent**: an account cannot be read at a past slot, so a node's answer names a different state than the fold does. Solana's `native::Bracket` is not one and is deliberately not called `Witness`: it is the batch's multi-account read pinned between two slot reads, an input to the send pipeline, never a vouch for a folded balance |
| `Indexer` | a type alias for `indexing::Service` over the chain's own `Source` and `BlockInterpreter`, built by `indexer(params)` | also hands `WalletFactory::new` its `indexing::OutputStore` (UTXOs) through `Indexer::outputs()` — the store's rows with this index's overlay folded in and its two gates taken | its `WalletFactory::new` registers an `indexing::Listener` that announces each committed checkpoint to submission recovery, so an indexing-only caller allocates no channel |
| `Submitter` | `broadcast` returns `native::Error`; `RpcUnavailable` means the outcome is unknown, and `sender/` attaches the local id as ambiguous; `known` asks for the exact id | `broadcast` takes the fee ceiling; `known` is `getrawtransaction` (mempool, or chain with `-txindex`) | every `broadcast` failure is unknown; `known` (signature status) and `height` settle unknown outcomes |
| Send pipeline | section 7 | outputs, not senders, are reserved | submissions run in a supervised task (`sender/registration.rs`) |
| Fee policy | named units | `FeeRate` is sat/kvB, built with `from_sat_per_vb` / `from_sat_per_kvb`; `WalletConfig` defaults to Core's `maxfeerate` | none: the node prices each message (`getFeeForMessage`), `Lamport` is the unit |
| `ErrorKind` | the same thirteen baseline variants everywhere, so no `base::TransactionErrorKind` is unreachable from a chain (each `error.rs` proves it in `every_kind_is_classified_and_no_transaction_kind_is_unreachable`) | baseline only | one extra variant, `Unsupported`: a Token-2022 mint refused at startup, and the account shapes this integration will not pay — a *kind* of failure the other two chains cannot have. **Not** an SPL wallet asked to author a payment: that was four runtime gates over one `WalletConfig::sendable`, and it is now a `FactoryProfile::authorship` of `None`, refused once by `wallets::Wallets` at lookup. A token wallet reaches no builder and no send pipeline to refuse |
| Amount receiver | the amount shape of section 3 | `Satoshi` is `Copy`, so its methods take `self` | `Lamport` is `Copy`, so its methods take `self`; `Wei` is 256 bits and borrows (`&self`). Each is the chain's atomic-unit count for *every* asset it carries — `Lamport` for a mint's units, `Wei` for an ERC-20's — with the asset naming the scale in both directions — `from_decimal(d, decimals)` inbound and `decimal_with(decimals)` outbound — and the native accessor (`decimal`) reserved for the native one |
| `AssetKind` | one enum in `native/asset.rs`, `id()` in `identity.rs` on every chain | one variant, `Native`, with `metadata()` (nothing else carries the asset) | two variants on both token chains: `Erc20(Address)` on ethereum, `SplToken(Address)` (the mint) on solana. A token names no name or ticker on chain, so it carries only decimals, on `WalletConfig`; solana keeps `metadata()` and returns `None` for a mint. Both spell a token asset the same way — `<chain>/<canonical token address>` — and both return `&AssetKind` from `WalletConfig::asset()` (no longer `Copy`, since it holds an address). Bitcoin has no `WalletConfig::asset()` at all — with one asset there is nothing to select |
| `WalletFactory::sibling` | only a chain with more than one asset has one: it is how a second asset shares one `Outbox`, so one sender is never reserved twice. Ethereum (ERC-20) and solana (SPL) both have one | **absent**: bitcoin is single-asset by construction, and its `Outbox` is built from the `WalletConfig` (network, address type, fee ceiling), so two configurations could not share one anyway (amends a retired design note (010) section 4.1) | **present** since SPL tokens: the sibling shares the RPC handle and the `Outbox`, so a wallet's SOL and its tokens are one reserved sender. Call it after `with_registrar`, which replaces the pipeline (supersedes its removal in a retired design note (018)) |
| `BlockInterpreter::inspect` order | one contract on all three: parse the evidence, then check the address filter — **always**, whatever the block turns out to hold. A block that cannot be read is `InvalidBlock` first; a filter of another network is `NetworkMismatch` even for a block with no transactions and no withdrawals. Ethereum used to build its address set lazily and returned `Ok` on a foreign filter for such a block while the other two refused it, so each chain's tests now cover the empty block (`docs/DEFECT_PATTERNS.md` section 2.3). The network guard below is the one thing that precedes even the parse | identical; its own empty-block test is `an_empty_block_still_rejects_an_address_filter_from_another_network` | identical; same test name |
| `ObservationDraft::transaction_order` | one contract on all three: the transaction's place in the **complete** block-ordered list, never its place in what `inspect` reported. `inspect` filters inside the loop on every chain, so the counter is the block's own; a store is handed the selection alone and could not recompute it, and a rank over the selection would move under a second, wider watch. Each chain's `a_draft_carries_its_place_in_the_block_not_in_the_selection` is that test | the position in the `tx` array, whose length the parse checks against the block's `nTx` | ethereum takes the node's own `transactionIndex`, already checked against block order and against the receipt (`native/block/head.rs`, `native/block/receipt.rs`); solana takes the position in the `transactions` array of a `transactionDetails: "full"` `getBlock` |
| Interpreter network guard | reject a block of another network when the evidence can say so | `RawBlock` records the network its verified client reported (it needs it to decode addresses), so `inspect` rejects a foreign block with `NetworkMismatch` | ethereum's and solana's raw block evidence carries no network identity, so there is nothing to compare; the `Index` owns the partition |
| `Batch` and `Settlement` | `sender.rs` is `Batch` on every chain, and is that chain's `wallets::Authorship`: the batch `Sender` and the `Author` upgrade of one of its own wallets; `Reservations::admit` opens the reserve stage on every chain | `Batch` carries the `WalletConfig`; `Settlement::accept`/`reject` settle one submission | `Batch` carries the `WalletConfig` too, and only a native SOL factory builds one — a token factory declares no authorship at all, so there is no token batch to refuse; `Batch::item` is a method beside the module-level `native_transfer`. One `Settlement` spans the whole batch inside the supervised task, so it has `retain(source)` — keep this one sender for recovery, release the rest — instead of `accept`/`reject` |
| `key.rs` | holds only `Signer`, and on every chain that is one `impl Signer for KeyPair` delegating to `self.pair.sign(request)` — byte-identical on all three, 12 to 16 production lines. The signing-request policy is `crypto::KeyPair`'s, decided by the key's own curve; no chain's `key.rs` states a rule of its own | delegates; the file's tests are what differ | delegates identically. The Ed25519 refusals (digest, secp256k1 scheme, key tweak, non-raw encoding) are `crypto::KeyPair`'s and are only *asserted* here |
| Test seam | test-only: `#[cfg(test)] pub(crate) mod stub;` on every chain | `Wallet::fixture` builds a wallet from doubles | `stub::factory` builds the *factory* instead, because a Solana wallet needs the indexer its factory holds |

## 7. Send pipeline

One name, one place (a retired design note (010)). Each `WalletFactory::new` builds one
`Arc<Outbox>` (`pub(crate)`; siblings share it) and every send path goes
through it:

| Contract | Calls |
|---|---|
| `FactoryProfile::authorship()` → `Batch` (`sender.rs`), which is both the `Sender` and the `Author` upgrade | `SendError::check_count`, one `Item` per transfer, `Outbox::send(items)` |
| `SingleSender::send` | `Wallet::item(to, amount)`, `Outbox::send(vec![item])` |
| `TransactionBuilder::draft` | nothing: typed intent plus the chain's snapshot, with no RPC and no key |
| `TransactionBuilder::prepare` | `Outbox::sign`: reserve → prepare → sign, then release and hold the signed transaction, returning it with the fee it pays (`base::SignedTransaction::fee`, atomic units of the chain's own currency: bitcoin inputs − outputs, ethereum `gas_limit × max_fee_per_gas` decoded from the signed envelope, solana `getFeeForMessage`) |
| `Broadcaster::broadcast` | `Outbox::broadcast`: a held transaction is reserved again and submitted; an unknown outcome is recovered |

An `Item` is `{ from, transfer, signer }`, built by
`Item::new(signer, from, transfer)` (ethereum adds its `FeeLimits`): the
sender is the signer's own address, and the requested `Transfer` has no
sender field. The adapter (`Wallet::item`, `Batch::item`) builds the native
`Transfer` and rejects a bad amount or destination before the outbox. Amounts
convert on the amount type, and a chain that carries more than one asset names
the scale in **both** directions: `Wei::from_decimal(d, decimals)`,
`Lamport::from_decimal(d, decimals)`, `Satoshi::from_decimal(d)` (bitcoin has
one asset and one scale), each rejecting zero. Solana's scale used to be an
unread `WalletConfig` field while `from_decimal` assumed SOL's nine, which is a
10^9 error waiting on the first SPL send (`docs/DEFECT_PATTERNS.md` section 1.1).

### Stages

Every chain uses these words in doc comments, test names, and private fns.

| Stage | Ethereum | Bitcoin | Solana |
|---|---|---|---|
| **reserve** | admit each sender; wait while one is busy; recover a sender's unknown outcome first | recover unknown outcomes of the senders; read indexed outputs at one checkpoint; select from outputs no other send holds and reserve the selection atomically | lease each sender (`SourceBusy` when taken) |
| **prepare** | one nonce range per sender (`Nonces`: `max(pending, floor)`); `Preflight::context` per item; aggregate `Funds` | runs before reserve: fee rate within the ceiling (`Preflight::fee_rate`); coin selection is `Builder::build` for one sender and `Builder::grouped` over one `Funds` per sender for several | account `Bracket`; blockhash `Lifetime`; fee per message; `Funds` |
| **sign** | `Builder::sign`; the exact envelopes are registered in the reservation | each input by its owner; `Preflight::accept` (`testmempoolaccept`) | sign; `simulate` |
| **submit** | in order, stop at the first failure (`dispatch.rs`) | one transaction (`dispatch.rs`) | in a registered task, up to three identical attempts (`dispatch.rs`) |
| **settle** | accept: nonce floor rises; definite: nonce freed; unknown: envelope kept, recovered lazily (`known`, else identical replay) | accept: outputs stay reserved until the index shows them spent; definite: freed; unknown: kept, recovered lazily (`known`, else identical replay; a refused replay frees them) | unknown: sender kept, recovered eagerly in the background (status, indexed history, expiry) |

### Files

Required on every chain (`linter.toml`): `sender/{outbox,reservation,dispatch,recovery}.rs`,
`native/rpc/submitter.rs`, `native/transaction/{preflight,funds}.rs`.

| File | Ethereum | Bitcoin | Solana |
|---|---|---|---|
| `sender.rs` | `Batch` | `Batch` | `Batch` |
| `sender/outbox.rs` | `Outbox`, `Item` | `Outbox`, `Item` | `Outbox`, `Item` |
| `sender/reservation.rs` | `Reservations` (senders, nonce floors, envelopes), `Admission`, `Settlement`, `PreparedTransaction` | `Reservations` (outpoints, envelopes), `Admission`, `Settlement`, `PreparedTransaction` | `Reservations` (senders, held envelopes), `Admission`, `Settlement`, `PreparedTransaction`. The hold is capped at `HELD = 64` on every chain; bitcoin and ethereum evict the oldest, solana the soonest to expire, because a Solana transaction stops being broadcastable at a known block height |
| `sender/dispatch.rs` | `Dispatch`: broadcast one `Settlement` | `Dispatch`: broadcast one `Settlement` | `Dispatch`: the task's ordered submit |
| `sender/recovery.rs` | `Recovery`: lazy | `Recovery`: lazy | `Recovery`: eager, background |
| `sender/stub.rs` | node double (tests) | node and index double (tests) | node and index double (tests) |
| chain-only | none | `sender/utxos.rs`: the index's spendable outputs | `sender/registration.rs`: `Spawner`, the supervised submission task (it *implements* `wallets::SubmissionRegistrar`; no type of that name exists in the crate) |
| `native/transaction/` sequencing rule | `nonce.rs` `Nonces` | none: outpoints are the resource | `lifetime.rs` `Lifetime` |

### Policies

| | Ethereum | Bitcoin | Solana |
|---|---|---|---|
| Same sender, concurrent | the second waits | disjoint outputs; `InsufficientFunds` only when none are left | the second fails with `SourceBusy` |
| Unknown outcome | `Unavailable` + local id | `Unavailable` + local id | `Unknown` + local id |
| A signed, never broadcast transaction | reserves nothing; its broadcast fails definitely once a later send took its nonce | reserves nothing; its broadcast fails definitely once a later send took an output | reserves nothing; its broadcast re-checks lifetime and sender |
| Errors of `send` / `sign` and `broadcast` | `SendError` / `TransactionError` | `SendError` / `TransactionError` | `SendError` / `TransactionError` (converted by `error.rs`) |
| Survives a restart | no | no | no |

The same-sender policy and the ambiguous kind differ by decision, not by
accident; unifying them is a product change (a retired design note (010) F3, F4).

## 8. Examples

`examples/` (package `payment-examples`) is where a user learns the public API.
One directory is one scenario, not one chain: a scenario is a single `main.rs`
registered as one `[[example]]`. No scenario reads a configuration file —
each states its chains as `Config` values in Rust, which is what proves a file
is sugar rather than the only door. Every
scenario sets up all three chains and then works through
`indexing::Index` and one `wallets::Wallets` keyed by the example's own
asset key alone, plus the scenario's own `local::Signers` map of what it can
sign, which is the thing the examples exist to show.

| Scenario | Shows |
|---|---|
| `wallet/main.rs` | section 4's construction per chain, through `sdk_config::Chains` + `wallets::Composition`: both startup phases, then `create`, a wallet built from held key material and proved with `accepts`, and `index.watch` for both |
| `transfer/main.rs` | the same setup, then fund each sender, one sync loop, the wallet's own `send`, balance, history |
| `indexing/main.rs` | the same setup, then a `Listener` registered with `index.listen`, `indexing::Supervisor::run` until every network is `Ready`, the checkpoint of every network, `index.watched`, the history, and the commits the listener queued |
| `watch_only/main.rs` | one observed address per chain: `parse` plus `index.watch` of a bare `indexing::Subject`, with history *and* balance served from the index, no wallet anywhere |
| `all_chains/main.rs` | the composition root in miniature: every chain's `Config` added to one `sdk_config::Chains` and verified, then one `wallets::Composition` over one index file, whose `Index`'s watch list is mirrored to the store (so a second run restores what is observed), the scenario's own key material (so a second run restores what can sign), a `base::SubmissionRegistrar` the process drains, one watched address, one sync loop, one send per chain |
| `local.rs` | non-chain setup only (the `local` library): the index directory, `local::compose` (both startup phases over one index file), the submission supervisor, JSON-RPC transport, raw node calls for funding |

Inside a scenario each chain is stated by one `fn <chain>()` returning that
chain's `Config`, and a chain crate is named nowhere else in the file.
`examples/tests/scenarios.rs` reads `sdk/chains/` and the `[[example]]` entries
and fails when a scenario omits a chain or reaches a chain crate outside that
function; `linter.toml` requires the tree, and `scripts/check-chains.sh`
check 4 repeats the correlation without cargo. Per-chain byte symmetry is no
longer the axis — the scenarios are chain-neutral after setup, and the setup
functions are now one expression each, differing exactly where section 6 says
the chains differ (the `Network` value and the endpoint). The `IndexerParams`,
`indexer(params)` and `WalletFactory::new` lines they used to repeat
belong to `Config::connect` + `Node::compose`, which `sdk_config::Chains` and
`wallets::Composition` drive.

Funding is a local-node call, never SDK API: bitcoin `generatetoaddress`
(101 blocks for coinbase maturity, one more to confirm the send), ethereum
Anvil's pre-funded key decoded with `crypto::SecretBytes::from_hex` and handed
to `Wallets::from(&ETH, secret)` (there is no `Wallets::import`), solana `requestAirdrop`
followed by a wait until the finalized balance appears.

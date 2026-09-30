//! A custom index store, from outside the SDK: this scenario supplies its own
//! repository ([`sink::Sink`]), observes addresses with birthdays through it,
//! drives `sync` itself, restarts over the same store, and asks its own
//! questions of it.
//!
//! It is deliberately the one scenario that does **not** go through
//! `sdk_config::Chains::compose` and `wallets::Composition`. That path also
//! builds wallet factories and asset units, and two of the three factories need
//! store capabilities this sink does not offer. What it uses instead is the
//! SDK's own assembly flow — `indexing::Index::new(store).chain(…).build()?` —
//! which is the whole of what composing an index requires: one store, one line
//! per chain, and a `build` that validates without launching anything.
//!
//! Its chains come from its own `config.toml` like every other scenario's, but
//! it decodes each section into its chain's own `Config` itself instead of
//! handing them to `sdk_config::Registry`, because the assembly a registry
//! performs is exactly what this scenario is showing.
//!
//! What it demonstrates, in order: two required ports and no others
//! (`sink.rs`), one index over one store assembled in the caller's own `main`, a
//! birthday in the past (`Birthday::At`) beside a fresh one (`Birthday::Fresh`), a
//! caller-driven bounded `sync` with no supervisor and no runner, a restart that
//! must restore the watch list and its progress from the store alone, and a
//! query API this consumer chose for itself.
//!
//! Needs all three local nodes (examples/README.md):
//! `cargo run -p payment-examples --example custom_store`

mod sink;

use std::{collections::BTreeMap, sync::Arc};

use indexing::{
    AssetId, Birthday, BlockPosition, Checkpoint, Index, Indexing, Network, Subject, Watch,
    Watching,
};
use json_rpc::Client as _;
use sdk_config::Document;
use serde_json::{Value, json};
use sink::Sink;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

/// How many bounded passes this scenario drives, per phase. `sync` returns when
/// its policy's batch is spent, so a caller that wants more calls it again.
const PASSES: usize = 3;

/// How far below the checkpoint the second observation of each chain starts.
/// A birthday in the past is the interesting one: it is the case that owes a
/// backfill, records coverage, and therefore has something to restore.
const BACKDATE: u64 = 4;

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. One store, held behind a reference count. It is not `Clone`, and it
    //    does not have to be: `indexing` implements every port for `Arc<T>`.
    let sink = Arc::new(Sink::new());

    // 2. One index over that one store, three chains. The networks are the
    //    ones `config.toml` states — the same one source every other scenario
    //    configures itself from — but assembled here by hand rather than by
    //    `sdk_config::Registry`, which is this scenario's whole point. No
    //    wallet map: two of the three factories need store capabilities this
    //    sink does not offer.
    //
    //    Local testing only: a `solana-test-validator --reset` is a new
    //    cluster every run, so `config.toml` cannot state its genesis hash and
    //    the node is asked for it. A deployment writes the hash it expects.
    let sections: Value = Document::toml(CONFIG).parse()?;
    let url = sections["solana"]["rpc"]["endpoints"][0]
        .as_str()
        .ok_or("[solana] states rpc.endpoints")?;
    let genesis: String = json_rpc::Endpoint::new(url)
        .connect(&BTreeMap::new())?
        .request(json_rpc::Request::once("getGenesisHash", json!([])))
        .await?
        .map_err(json_rpc::CallError::from)?
        .deserialize()?;
    let document = CONFIG.replace("<genesis-hash-from-the-node>", &genesis);
    let sections: Value = Document::toml(document).parse()?;
    let (index, units) = assemble(&sections, &sink).await?;

    // 3. Observe, from the block after each network's checkpoint.
    for unit in &units {
        unit.observe(&index).await?;
    }

    // 4. Drive. Bounded by contract: each call does the work its policy allows
    //    and returns, and this loop is the whole scheduler. One call advances
    //    every chain, and one chain's failure does not cost the others theirs.
    drive(&index, PASSES).await?;

    // 5. Now observe a second address from a birthday the network has already
    //    passed, and drive again so the index records what it read.
    for unit in &units {
        unit.backdate(&index).await?;
    }
    drive(&index, PASSES).await?;

    // 6. Ask the store the questions this consumer actually has.
    report(&sink, &units);

    // 7. Drop the index and rebuild over the same store. This is the assertion
    //    that catches the most.
    restart(&sections, &sink, index, units).await
}

/// One chain network as this scenario observes it: the network, and the two
/// addresses it watches there.
///
/// Nothing in it names a chain, and it holds no index: there is one index for
/// every network now, and this is only what to ask it about.
struct Observer {
    network: Network,
    /// Observed from `Birthday::Fresh`, then from `Birthday::At` a block the network
    /// has already passed.
    subjects: [Subject; 2],
}

impl Observer {
    /// One network's unit and the two addresses this scenario observes on it.
    fn new(network: Network, addresses: [&str; 2]) -> Self {
        let asset = AssetId::native(network.chain.clone());
        Self {
            subjects: addresses.map(|text| Subject {
                address: base::Address::new(network.clone(), text),
                asset: asset.clone(),
            }),
            network,
        }
    }

    /// The ids this scenario files its two observations under.
    fn ids(&self) -> [String; 2] {
        [
            format!("{}-fresh", self.network.chain),
            format!("{}-past", self.network.chain),
        ]
    }

    /// Observes the first address from the block after the current checkpoint.
    ///
    /// `Birthday::Fresh` is an assertion of fact — this address has never been used
    /// — so it is the honest one for an address this process just chose, and the
    /// wrong one for anything that may already have a history.
    ///
    /// # Errors
    ///
    /// Whatever the store refuses, and a conflicting re-registration.
    async fn observe(&self, index: &Index<dyn Indexing>) -> Result<Watch, AnyError> {
        let [id, _] = self.ids();
        let watch = index.watch(&id, &self.subjects[0], Birthday::Fresh).await?;
        println!(
            "{id}: observing {} from {:?}",
            watch.address.value, watch.start
        );
        Ok(watch)
    }

    /// Observes the second address from [`BACKDATE`] blocks below the
    /// checkpoint, which is the case that still has its own past to read.
    ///
    /// # Errors
    ///
    /// Whatever the store refuses, and `InvalidRequest` when the birthday is
    /// further below the checkpoint than the chain's policy allows.
    async fn backdate(&self, index: &Index<dyn Indexing>) -> Result<Watch, AnyError> {
        let [_, id] = self.ids();
        let start = index
            .checkpoint(&self.network)
            .await?
            .map_or(BlockPosition(0), |block| {
                BlockPosition(block.position.0.saturating_sub(BACKDATE))
            });
        let watch = index
            .watch(&id, &self.subjects[1], Birthday::At(start))
            .await?;
        println!(
            "{id}: observing {} from {:?}",
            watch.address.value, watch.start
        );
        Ok(watch)
    }

    /// Everything the store has to restore for this network after a restart:
    /// each observation as it was registered, and the first block each one still
    /// owes.
    ///
    /// # Errors
    ///
    /// Whatever the store reports reading its own watch list back.
    async fn restored(
        &self,
        index: &Index<dyn Indexing>,
        sink: &Sink,
    ) -> Result<Snapshot, AnyError> {
        Ok(Snapshot {
            watches: index.watched(&self.network).await?,
            checkpoint: index
                .checkpoint(&self.network)
                .await?
                .map(|block| block.position),
            owed: sink.catching_up(&self.network),
        })
    }
}

/// What a restart must reproduce for one network.
///
/// Compared as one value rather than field by field, so a restart that restores
/// the watches but loses their coverage fails as loudly as one that restores
/// nothing.
#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    watches: Vec<Watch>,
    checkpoint: Option<BlockPosition>,
    owed: Vec<(String, BlockPosition)>,
}

/// The two addresses this scenario observes on each chain, in the order the
/// chains are assembled.
const OBSERVED: [[&str; 2]; 3] = [
    [
        "bcrt1qw508d6qejxtdg4y5r3zarvary0c5xw7kygt080",
        "bcrt1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3qccfmv3",
    ],
    [
        "0x70997970c51812dc3a010c7d01b50e0d17dc79c8",
        "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc",
    ],
    [
        "So11111111111111111111111111111111111111112",
        "SysvarRent111111111111111111111111111111111",
    ],
];

/// What one chain contributes to the assembly: its verified node, as a
/// constructor still waiting for the store.
///
/// One of these per chain, built by `Chain::<chain>` below, so the chain crate is
/// named in one place and the assembly names none of them.
struct Chain(Composing);

/// One chain's index, once the store reaches it.
type Composing = Box<dyn FnOnce(Arc<Sink>) -> Result<Arc<dyn Indexing>, indexing::IndexError>>;

impl Chain {
    /// This chain's index over `store`.
    ///
    /// # Errors
    ///
    /// Whatever the chain refuses about its own indexing policy.
    fn compose(self, store: Arc<Sink>) -> Result<Arc<dyn Indexing>, indexing::IndexError> {
        (self.0)(store)
    }

    /// Bitcoin: a regtest node, checked against the network it serves.
    ///
    /// `RpcClient::connect` verifies that network and opens no store;
    /// `IndexerParams::new` takes the store as it is, and this chain's own
    /// `indexer(params)` constructor builds the index. What this scenario cannot
    /// reach is `WalletFactory::new`: that one takes the index *as* an
    /// `indexing::OutputStore`, which it is only while the store is one, so coin
    /// selection makes live outputs a hard requirement of *making a wallet* on
    /// this chain rather than a capability.
    /// The addresses in [`OBSERVED`] are therefore text this scenario states, not
    /// text a factory parsed.
    ///
    /// # Errors
    ///
    /// The node serves another network.
    async fn bitcoin(sections: &Value) -> Result<Self, AnyError> {
        let config: chain_bitcoin::Config<String> =
            serde_json::from_value(sections["bitcoin"].clone())?;
        let transport = config.rpc.transport(&BTreeMap::new())?;
        let rpc = chain_bitcoin::RpcClient::connect(transport, &config.network).await?;
        Ok(Self(Box::new(|store| {
            chain_bitcoin::indexer(chain_bitcoin::IndexerParams::new(rpc, store))
                .map(|index| Arc::new(index) as Arc<dyn Indexing>)
        })))
    }

    /// Ethereum: an Anvil node, checked against its EIP-155 chain id.
    ///
    /// The one chain whose wallet factory a minimal store could reach:
    /// `WalletFactory::new` takes only the RPC client. This scenario still
    /// observes plain `Subject`s, so that all three chains read alike.
    ///
    /// # Errors
    ///
    /// The node serves another chain id.
    async fn ethereum(sections: &Value) -> Result<Self, AnyError> {
        let config: chain_ethereum::Config<String> =
            serde_json::from_value(sections["ethereum"].clone())?;
        let network = chain_ethereum::Network::new(config.chain_id, config.network.as_str());
        let transport = config.rpc.transport(&BTreeMap::new())?;
        let rpc = chain_ethereum::RpcClient::connect(transport, &network).await?;
        Ok(Self(Box::new(|store| {
            chain_ethereum::indexer(chain_ethereum::IndexerParams::new(rpc, store))
                .map(|index| Arc::new(index) as Arc<dyn Indexing>)
        })))
    }

    /// Solana: a test validator, checked against the genesis hash its section
    /// states — which `main` has already filled in from the node, because a
    /// `--reset` cluster has no hash a checked-in file could name.
    ///
    /// `WalletFactory::new` here takes a history reader beside the index, so
    /// this chain's wallets need a store whose own `ConfirmedHistory` query a
    /// reader can be built over; the index does not.
    ///
    /// # Errors
    ///
    /// The node serves another cluster.
    async fn solana(sections: &Value) -> Result<Self, AnyError> {
        let config: chain_solana::Config<String> =
            serde_json::from_value(sections["solana"].clone())?;
        let network = chain_solana::Network::new(config.network.as_str(), config.genesis_hash);
        let transport = config.rpc.transport(&BTreeMap::new())?;
        let rpc = chain_solana::RpcClient::connect(transport, &network).await?;
        Ok(Self(Box::new(|store| {
            chain_solana::indexer(chain_solana::IndexerParams::new(rpc, store))
                .map(|index| Arc::new(index) as Arc<dyn Indexing>)
        })))
    }
}

/// One index over one store, and the units this scenario observes on it.
///
/// **Two phases, and they never merge.** Every node's identity is verified first,
/// before the store is handed to anything: a deployment that names the wrong node
/// fails without having written a byte. Then one `Index::over(store)`, one
/// `.chain(…)` per chain, and a `build` that refuses a duplicate network without
/// launching a task.
///
/// `Index<dyn Indexing>` is the only shape there is: a composition routes no query
/// at all. This store answers none either — no balance, no page, no spendable
/// set — and nothing here asks an index for one, because each of those is the
/// repository's own answer joined to an index's pending blocks. Routing watches,
/// reporting checkpoints and driving `Index::sync` need none of that.
///
/// # Errors
///
/// The first node that does not serve the network its setup states, whatever a
/// chain refuses about its own indexing policy, and two chains claiming one
/// network.
async fn assemble(
    sections: &Value,
    sink: &Arc<Sink>,
) -> Result<(Index<dyn Indexing>, Vec<Observer>), AnyError> {
    let (first, second, third) = (
        Chain::bitcoin(sections).await?,
        Chain::ethereum(sections).await?,
        Chain::solana(sections).await?,
    );
    let index = Index::over(Arc::clone(sink))
        .chain(|store| first.compose(store))?
        .chain(|store| second.compose(store))?
        .chain(|store| third.compose(store))?
        .build()?;
    let units = index
        .networks()
        .iter()
        .cloned()
        .zip(OBSERVED)
        .map(|(network, addresses)| Observer::new(network, addresses))
        .collect();
    Ok((index, units))
}

/// Drives the whole index `passes` times.
///
/// This is the whole of the scheduling an embedder needs. `Index::sync` is
/// bounded by contract — one pass reads at most what each network's `SyncPolicy`
/// allows and returns — so nothing here has to cancel, retry or wait, and no
/// supervisor is involved. Every chain is asked on every pass, and one chain's
/// refusal does not cost the others theirs; this loop stops at the worst of them,
/// which is the simplest policy and a deliberate choice rather than a default.
///
/// # Errors
///
/// The worst failure any network reported.
async fn drive(index: &Index<dyn Indexing>, passes: usize) -> Result<(), AnyError> {
    for pass in 1..=passes {
        for status in index.sync().await? {
            let at = status.checkpoint.map(|block| block.position.0);
            println!(
                "pass {pass} {}/{}: {:?}, checkpoint {at:?}",
                status.network.chain, status.network.name, status.phase
            );
        }
    }
    Ok(())
}

/// Prints what the store holds, in the store's own vocabulary.
///
/// Not a history page and not a balance: this consumer asked its own repository
/// three questions it chose, and none of them is a shape `apps/server` serves.
fn report(sink: &Sink, units: &[Observer]) {
    for unit in units {
        for (id, start) in sink.observed(&unit.network) {
            println!("{id}: observed from block {}", start.0);
        }
        for (id, start) in sink.catching_up(&unit.network) {
            println!("{id}: still reading its own past from block {}", start.0);
        }
        for (id, landing) in sink.landings(&unit.network) {
            println!(
                "{id}: {} landed over {} block(s), newest {}",
                landing.credited, landing.blocks, landing.newest.0
            );
        }
    }
}

/// Drops every index and rebuilds over the same store, then checks that what
/// came back is what went away.
///
/// The index holds each network's watch list in memory, because the publication
/// handshake captures it synchronously; the store is the only thing that
/// survives it. So
/// this is the assertion that a third-party store is really load-bearing: if
/// `WatchStore::all` returned the watches without their coverage, or forgot the
/// birthday it was given, or handed back a watch paired with another watch's
/// progress, the comparison below is what says so.
///
/// # Errors
///
/// Whatever reconnecting reports, and a restart that restored something else.
async fn restart(
    sections: &Value,
    sink: &Arc<Sink>,
    index: Index<dyn Indexing>,
    units: Vec<Observer>,
) -> Result<(), AnyError> {
    let mut before = Vec::with_capacity(units.len());
    for unit in &units {
        before.push(unit.restored(&index, sink).await?);
    }
    drop((index, units));

    let (index, units) = assemble(sections, sink).await?;
    for (unit, before) in units.iter().zip(before) {
        let after = unit.restored(&index, sink).await?;
        assert_eq!(
            after, before,
            "{}/{}: a restart over the same store must restore every observation, \
             its birthday and its recorded progress",
            unit.network.chain, unit.network.name
        );
        println!(
            "{}/{}: {} observation(s) restored from the store alone",
            unit.network.chain,
            unit.network.name,
            after.watches.len()
        );
    }
    Ok(())
}

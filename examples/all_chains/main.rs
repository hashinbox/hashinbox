//! Every chain at once, in the shape of the `payment-server` composition root
//! (`apps/server/src/chains.rs`): each section of `config.toml` is parsed and
//! accepted by the chain registered for it, each connects and verifies its
//! node, and one [`Composition`] turns the verified chains into one
//! `indexing::Index`, one `wallets::Wallets<Asset>` and one polling cadence
//! per network, over one index file. Then one submission supervisor, one sync
//! loop, one watched address, and one send per chain.
//!
//! The two phases are the whole startup rule: **every identity check finishes
//! before storage is opened**, which is why the chains are verified into a
//! vector first and the store is opened after it.
//!
//! The two halves of "what comes back after a restart" have different owners
//! (`docs/design/030 §8 (ex-029)` section 3.4). What is *observed* comes back from the
//! index's own registry rows, at no cost to this file. What can *sign* comes
//! back from the application's key material — here, fixed seeds; in a server,
//! a key store. The server reaches this from its own document the same way;
//! what it adds is an HTTP surface and shutdown supervision, not structure.
//! Needs all three local nodes
//! (examples/README.md): `cargo run -p payment-examples --example all_chains`

use std::{sync::Arc, time::Duration};

use base::BlockPosition;
use indexing::{ObservedTransaction, Page, Start, Subject, Supervisor, Watching as _};
use local::{AnyError, Asset, Health, Scenario, Signers, Submissions};
use serde_json::json;
use tokio::sync::watch;
use wallets::{Composition, SecretBytes, SubmissionRegistrar};

const INTERVAL: Duration = Duration::from_secs(1);
/// Blocks mined to bitcoin's sender: a coinbase output matures after 100.
const MATURITY: u32 = 101;
/// One SOL in lamports.
const AIRDROP: u64 = 1_000_000_000;
/// Anvil's first pre-funded development account (public test key).
const ANVIL_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// A well-known Ethereum address, observed without a key.
const WATCHED: &str = "0x000000000000000000000000000000000000dead";
const WATCHED_ID: &str = "eth-watched";

/// Each chain's part: its asset tag, the sending wallet, the receiving wallet,
/// and the amount sent in display units.
const TRANSFERS: [(Asset, &str, &str, &str); 3] = [
    (Asset::Btc, "btc-alice", "btc-bob", "0.5"),
    (Asset::Eth, "eth-alice", "eth-bob", "0.01"),
    (Asset::Sol, "sol-alice", "sol-bob", "0.1"),
];

/// This scenario's stand-in for a key store: one fixed seed per wallet id, so
/// a second run rebuilds the same wallets instead of minting new ones. A
/// server reads its keys from configuration instead; either way, the index
/// never holds them.
const SEEDS: [(&str, u8); 5] = [
    ("btc-alice", 0x11),
    ("btc-bob", 0x12),
    ("eth-bob", 0x13),
    ("sol-alice", 0x14),
    ("sol-bob", 0x15),
];

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. The document's sections, each accepted by the chain registered for
    //    it, then both startup phases in the one order that is safe: every
    //    node identity is verified, storage is opened only then, and each
    //    chain composes itself over its own network's repository. This is
    //    exactly what `apps/server` does with its own document.
    let scenario = Scenario::read("all_chains").await?;
    let submissions = Arc::new(Submissions::default());
    let registrar: Arc<dyn SubmissionRegistrar> = submissions.clone();
    let Composition {
        index,
        history,
        balance: balances,
        wallets,
        units,
        schedules,
        ..
    } = scenario.compose(&registrar).await?;

    // 2. What a previous run observed is already back: the index read its own
    //    rows before its first pass, and this file did nothing for it.
    for network in index.networks() {
        let watched = index.watched(network).await?;
        println!("{}: {} address(es) restored", network.chain, watched.len());
    }

    // 3. A sender and a receiver per chain, rebuilt from this scenario's own
    //    key material. Ethereum's sender is Anvil's pre-funded account.
    //    Re-watching an address under the same id and start is a no-op, so a
    //    second run states the same thing and nothing changes.
    let mut signers = Signers::default();
    let alice = wallets
        .from(&Asset::Eth, SecretBytes::from_hex(ANVIL_KEY)?)
        .await?;
    index
        .watch("eth-alice", alice.as_ref(), Start::At(BlockPosition(0)))
        .await?;
    signers.keep("eth-alice", alice);
    for (asset, sender, receiver, _) in TRANSFERS {
        for id in [sender, receiver] {
            if signers.get(id).is_ok() {
                continue;
            }
            let wallet = wallets.from(&asset, seed(id)?).await?;
            index.watch(id, wallet.as_ref(), Start::Fresh).await?;
            signers.keep(id, wallet);
        }
    }

    // 4. One watched address: history only, no key, no balance, no sends.
    let eth = wallets.asset(&Asset::Eth)?;
    index
        .watch(
            WATCHED_ID,
            &Subject {
                address: wallets.parse(&Asset::Eth, WATCHED)?,
                asset: eth.id.clone(),
            },
            Start::Fresh,
        )
        .await?;
    fund(&scenario, &signers).await?;

    // 5. One sync loop over the composer serves every address, watched or not.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = Health::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    readiness.ready().await?;

    // 6. One send per chain, from the wallet this file holds.
    for (asset, sender, receiver, amount) in TRANSFERS {
        while balances
            .balance(&**signers.get(sender)?)
            .await?
            .amount
            .is_zero()
        {
            tokio::time::sleep(INTERVAL).await;
        }
        let text = signers.get(receiver)?.address().value.clone();
        let to = wallets.parse(&asset, &text)?;
        // Spending is the asset's capability: the map hands out an author
        // only for an asset this system sends, and never for one it only
        // receives.
        let author = wallets.author(&asset, signers.get(sender)?)?;
        let id = author.send(to, amount.parse()?).await?;
        println!("{sender} sent {amount} to {receiver} in {id}");
    }

    // 7. Stop the loop, then drain the supervisor: a send whose chain finishes
    //    it in the background must not be cut off by the process exiting.
    let watched = transactions(
        history.as_ref(),
        &eth.id,
        &wallets.parse(&Asset::Eth, WATCHED)?,
    )
    .await?;
    let unit = units.of(&eth.id).ok_or("ether's unit")?;
    println!(
        "{WATCHED}: {} transaction(s), fees in {} digits",
        watched.len(),
        unit.decimals
    );
    stop.send(true)?;
    sync.await?;
    submissions.drain().await
}

/// This scenario's key material for `id`: a fixed 32-byte seed.
fn seed(id: &str) -> Result<SecretBytes, AnyError> {
    let byte = SEEDS
        .iter()
        .find(|(known, _)| *known == id)
        .map(|(_, byte)| *byte)
        .ok_or_else(|| format!("no key material for {id}"))?;
    Ok(SecretBytes::new(vec![byte; 32]))
}

/// The confirmed half of one holding's history, exactly as the reader the
/// composition handed back answers it: atomic amounts, which the asset's own
/// unit scales when one is printed.
async fn transactions(
    history: &dyn indexing::History,
    asset: &base::AssetId,
    address: &base::Address,
) -> Result<Vec<ObservedTransaction>, AnyError> {
    let subject = Subject {
        address: address.clone(),
        asset: asset.clone(),
    };
    Ok(history
        .history(&subject, Page::first(10))
        .await?
        .transactions)
}

/// Funds the bitcoin and solana senders through the nodes the document named;
/// the ethereum sender is Anvil's pre-funded account.
///
/// # Errors
///
/// The document configures no such section, or the node refuses the call.
async fn fund(scenario: &Scenario, signers: &Signers) -> Result<(), AnyError> {
    let coinbase = signers.get("btc-alice")?.address().value.clone();
    let _: Vec<String> = scenario
        .node("bitcoin")?
        .call("generatetoaddress", json!([MATURITY, coinbase]))
        .await?;
    let airdropped = signers.get("sol-alice")?.address().value.clone();
    let _: String = scenario
        .node("solana")?
        .call("requestAirdrop", json!([airdropped, AIRDROP]))
        .await?;
    Ok(())
}

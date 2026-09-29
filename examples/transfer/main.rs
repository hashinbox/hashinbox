//! A transfer on every chain at once: fund a sender, run one sync loop, send
//! through the wallet each factory handed back, then read the sender's balance
//! and the receiver's history. Funding is always a local-node call, never SDK
//! API. Needs all three local nodes (examples/README.md):
//! `cargo run -p payment-examples --example transfer`

use std::{sync::Arc, time::Duration};

use base::BlockPosition;
use indexing::Supervisor;
use indexing::{ObservedTransaction, Page, Start, Watching as _};
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

/// Each chain's part: its asset tag, the sending wallet, the receiving wallet,
/// and the amount sent in display units.
const TRANSFERS: [(Asset, &str, &str, &str); 3] = [
    (Asset::Btc, "btc-alice", "btc-bob", "0.5"),
    (Asset::Eth, "eth-alice", "eth-bob", "0.01"),
    (Asset::Sol, "sol-alice", "sol-bob", "0.1"),
];

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Both phases, in order: `config.toml` says what this scenario runs,
    //    `connect` checks that each node serves what its section claims, and
    //    the store opens only once every identity has held.
    let scenario = Scenario::read("transfer").await?;
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
    // What each asset is, from the map, beside what an amount of it is, from
    // the chain that declared it.
    let mut declared = Vec::new();
    for (asset, ..) in TRANSFERS {
        let id = wallets.asset(&asset)?.id;
        let unit = units.of(&id).ok_or("a declared unit")?.clone();
        declared.push((id, unit));
    }

    // 2. A receiver per asset, then the senders. Ethereum's sender is Anvil's
    //    pre-funded account, built from its key and watched from block zero —
    //    an explicit birthday, which is what `Start::At` states.
    let mut signers = Signers::default();
    for (asset, _, receiver, _) in TRANSFERS {
        let wallet = wallets.create(&asset).await?;
        index.watch(receiver, wallet.as_ref(), Start::Fresh).await?;
        signers.keep(receiver, wallet);
    }
    let secret = SecretBytes::from_hex(ANVIL_KEY)?;
    let alice = wallets.from(&Asset::Eth, secret).await?;
    index
        .watch("eth-alice", alice.as_ref(), Start::At(BlockPosition(0)))
        .await?;
    signers.keep("eth-alice", alice);
    for (asset, sender) in [(Asset::Btc, "btc-alice"), (Asset::Sol, "sol-alice")] {
        let wallet = wallets.create(&asset).await?;
        index.watch(sender, wallet.as_ref(), Start::Fresh).await?;
        signers.keep(sender, wallet);
    }

    // 3. Funding is a local-network operation and not part of the SDK, so it
    //    goes straight to the node the document named: bitcoin mines past
    //    coinbase maturity, solana asks for an airdrop.
    let coinbase = signers.get("btc-alice")?.address().value.clone();
    let _: Vec<String> = scenario
        .node("bitcoin")?
        .call("generatetoaddress", json!([MATURITY, &coinbase]))
        .await?;
    let airdropped = signers.get("sol-alice")?.address().value.clone();
    let _: String = scenario
        .node("solana")?
        .call("requestAirdrop", json!([airdropped, AIRDROP]))
        .await?;

    // 4. One sync loop over the composed index serves every chain's wallets;
    //    wait until every network has reached its node's tip.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = Health::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    readiness.ready().await?;

    // 5. One send per chain: parse the receiver's address text in the
    //    sender's own network, then send display units.
    for ((asset, sender, receiver, amount), _) in TRANSFERS.into_iter().zip(&declared) {
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

    // 6. Only a confirmed transaction is history, so mine bitcoin's block;
    //    Anvil and the validator confirm on their own.
    let _: Vec<String> = scenario
        .node("bitcoin")?
        .call("generatetoaddress", json!([1, &coinbase]))
        .await?;
    for ((_, sender, receiver, _), (asset, unit)) in TRANSFERS.into_iter().zip(&declared) {
        let mut page =
            transactions(history.as_ref(), asset, signers.get(receiver)?.address()).await?;
        while page.is_empty() {
            tokio::time::sleep(INTERVAL).await;
            page = transactions(history.as_ref(), asset, signers.get(receiver)?.address()).await?;
        }
        let settled = balances.balance(&**signers.get(sender)?).await?;
        let balance = unit.display(&settled.amount)?;
        println!(
            "{sender} balance {balance}; {receiver} has {} transaction(s)",
            page.len()
        );
    }
    // 7. Stop the loop, then drain the supervisor: a send whose chain finishes
    //    it in the background must not be cut off by the process exiting.
    stop.send(true)?;
    sync.await?;
    submissions.drain().await
}

/// The confirmed half of one holding's history, exactly as the reader the
/// composition handed back answers it: atomic amounts, which the asset's own
/// unit scales when one is printed.
async fn transactions(
    history: &dyn indexing::History,
    asset: &base::AssetId,
    address: &base::Address,
) -> Result<Vec<ObservedTransaction>, AnyError> {
    let subject = indexing::Subject {
        address: address.clone(),
        asset: asset.clone(),
    };
    Ok(history
        .history(&subject, Page::first(10))
        .await?
        .transactions)
}

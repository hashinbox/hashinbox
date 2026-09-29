//! Watch-only addresses: observe an address on every chain without holding a
//! key for it. `index.watch` takes any `indexing::Holder` — a wallet, or a
//! bare `Subject` of address and asset — the sync loop selects it like any
//! wallet, and the index serves its history. There is nothing to read a
//! balance with, because there is no key. Needs all three local nodes
//! (examples/README.md): `cargo run -p payment-examples --example watch_only`

use std::sync::Arc;

use indexing::Supervisor;
use indexing::{Page, Start, Subject, Watching as _};
use local::{AnyError, Asset, Health, Scenario, Submissions};
use tokio::sync::watch;
use wallets::{Composition, SecretBytes, SubmissionRegistrar};

/// Each chain's asset tag, the id its observed address is filed under, and the
/// seed the address is derived from. The wallet is dropped as soon as its text
/// is read, so this process keeps only the address — exactly what it would
/// have if the address belonged to someone else.
const OBSERVED: [(Asset, &str, u8); 3] = [
    (Asset::Btc, "btc-observed", 0x31),
    (Asset::Eth, "eth-observed", 0x32),
    (Asset::Sol, "sol-observed", 0x33),
];

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Both phases, in order: `config.toml` says what this scenario runs,
    //    and the store opens only once every node identity has held. `units`
    //    is what each chain declared an amount of its asset to be; no wallet
    //    carries it.
    let registrar: Arc<dyn SubmissionRegistrar> = Arc::new(Submissions::default());
    let Composition {
        index,
        history,
        balance: balances,
        wallets,
        units,
        schedules,
        ..
    } = Scenario::read("watch_only")
        .await?
        .compose(&registrar)
        .await?;

    // 2. Observe one address per asset. `parse` turns user-supplied text into
    //    the asset network's canonical address; `Subject` is that address and
    //    the asset to read it in, which is all a watch needs — a wallet is
    //    simply a `Holder` that can also sign.
    let mut subjects = Vec::new();
    for (asset, id, seed) in OBSERVED {
        let text = wallets
            .from(&asset, SecretBytes::new(vec![seed; 32]))
            .await?
            .address()
            .value
            .clone();
        let subject = Subject {
            address: wallets.parse(&asset, &text)?,
            asset: wallets.asset(&asset)?.id,
        };
        index.watch(id, &subject, Start::Fresh).await?;
        println!("{id}: watching {}", subject.address.value);
        let unit = units.of(&subject.asset).ok_or("a declared unit")?.clone();
        subjects.push((unit, subject));
    }

    // 3. The sync loop selects a watched address exactly like a wallet's; the
    //    supervisor takes no selection, because each unit reads its own list,
    //    and each chain keeps the cadence its own section stated.
    let (stop, stopped) = watch::channel(false);
    let (monitor, mut readiness) = Health::watching(index.networks());
    let sync = tokio::spawn(Supervisor::new(index.clone(), schedules, monitor).run(stopped));
    readiness.ready().await?;

    // 4. History and balance are both served for a watched address, each by the
    //    handle that owns its network rather than through the index: they are
    //    the repository's own answers completed by the blocks above its line,
    //    and neither needs a key.
    for ((_, id, _), (unit, subject)) in OBSERVED.into_iter().zip(&subjects) {
        let page = history.history(subject, Page::first(10)).await?;
        let balance = balances.balance(subject).await?;
        println!(
            "{id}: {} transaction(s), {} settled, watch_only=true",
            page.transactions.len(),
            unit.display(&balance.amount)?,
        );
    }
    stop.send(true)?;
    Ok(sync.await?)
}

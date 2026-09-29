//! Wallets, on every chain at once: compose the chains this scenario's
//! `config.toml` states, then make wallets through
//! `wallets::Wallets` alone — one created by its factory, one attached from a
//! locally generated `KeyPair` and accepted by that factory. The map hands each
//! wallet back and keeps nothing; this file keeps them. No funds move, and
//! nothing is persisted beyond the index. Needs all three local nodes
//! (examples/README.md): `cargo run -p payment-examples --example wallet`

use std::sync::Arc;

use indexing::{Start, Watching as _};
use local::{AnyError, Asset, Scenario, Signers, Submissions};
use wallets::{Composition, SecretBytes, SubmissionRegistrar};

/// One chain's asset tag, the wallet its factory generates, the wallet built
/// from key material this process holds, and the fixed seed that material is.
const WALLETS: [(Asset, &str, &str, u8); 3] = [
    (Asset::Btc, "btc-generated", "btc-attached", 0x21),
    (Asset::Eth, "eth-generated", "eth-attached", 0x22),
    (Asset::Sol, "sol-generated", "sol-attached", 0x23),
];

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Both phases, in order: `config.toml` says what this scenario runs,
    //    `connect` verifies each node's identity, and the store opens only
    //    once every identity has held. What comes back is one composed index
    //    over every network and one factory per asset tag.
    let registrar: Arc<dyn SubmissionRegistrar> = Arc::new(Submissions::default());
    let Composition { index, wallets, .. } =
        Scenario::read("wallet").await?.compose(&registrar).await?;

    // 2. Make wallets. The map returns each one and keeps nothing, so this
    //    file holds them. Only the tag says which chain a wallet is on;
    //    nothing below names one.
    let mut signers = Signers::default();
    for (asset, created, attached, seed) in WALLETS {
        let held = wallets
            .from(&asset, SecretBytes::new(vec![seed; 32]))
            .await?;
        // An existing wallet is proved to belong to the asset by its factory,
        // not by a tag recorded when it was filed.
        wallets.accepts(&asset, held.as_ref())?;
        for (id, wallet) in [(created, wallets.create(&asset).await?), (attached, held)] {
            // The index's row commits first, then the key is kept: a crash
            // between the two leaves a watch-only address, never a key
            // nothing observes (`docs/design/030 §8 (ex-029)` section 3.3).
            index.watch(id, wallet.as_ref(), Start::Fresh).await?;
            signers.keep(id, wallet);
        }
    }

    // 3. What the index observes, and which of those this process can sign for.
    for network in index.networks() {
        for watch in index.watched(network).await? {
            let signing = signers.get(&watch.id).is_ok();
            println!(
                "{:<14} {:<10} {} signing={signing}",
                watch.id, watch.asset.asset, watch.address.value
            );
        }
    }
    Ok(())
}

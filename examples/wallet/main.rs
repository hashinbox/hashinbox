//! Wallets, on every chain at once: make one through its factory, attach one
//! from key material this process holds, and have the index observe both.
//! `wallets::Wallets` hands each wallet back and keeps nothing; this file keeps
//! them. No funds move. Needs all three local nodes (examples/README.md):
//! `cargo run -p payment-examples --example wallet`

use std::{collections::BTreeMap, sync::Arc};

use indexing::{
    Birthday, Watching as _,
    sqlite::{Database, Repository},
};
use json_rpc::Client as _;
use payments::config::{Composition, Document, Registry};
use serde_json::{Value, json};
use wallets::{SecretBytes, SubmissionRegistrar, Submissions, Wallet};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// The chains this scenario runs and the node of each.
const CONFIG: &str = include_str!("config.toml");

/// One chain's asset tag, the wallet its factory generates, the wallet built
/// from key material this process holds, and the fixed seed that material is.
const WALLETS: [(&str, &str, &str, u8); 3] = [
    ("btc", "btc-generated", "btc-attached", 0x21),
    ("eth", "eth-generated", "eth-attached", 0x22),
    ("sol", "sol-generated", "sol-attached", 0x23),
];

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    // 1. Local testing only: a `solana-test-validator --reset` is a new
    //    cluster every run, so `config.toml` cannot state its genesis hash and
    //    the node is asked for it. A deployment writes the hash it expects.
    let sections: Value = Document::toml(CONFIG).parse()?;
    let url = sections["solana"]["rpc"]["endpoints"][0]
        .as_str()
        .ok_or("[solana] states rpc.endpoints")?;
    let genesis: String = json_rpc::Endpoint::new(url)
        .connect()?
        .request(json_rpc::Request::once("getGenesisHash", json!([])))
        .await?
        .map_err(json_rpc::CallError::from)?
        .deserialize()?;
    let document = Document::toml(CONFIG.replace("<genesis-hash-from-the-node>", &genesis));

    // 2. The chains this application links, one line each. Each parses the
    //    section registered under its name; asset tags are this file's own.
    let mut registry = Registry::<String, Repository>::new();
    registry.register::<chain_bitcoin::Config<String>>("bitcoin");
    registry.register::<chain_ethereum::Config<String>>("ethereum");
    registry.register::<chain_solana::Config<String>>("solana");
    let chains = registry.chains(&document.parse()?)?;

    // 3. Compose. Every node's identity is verified before the store is
    //    opened, then each chain builds its index and factories over it.
    let registrar: Arc<dyn SubmissionRegistrar> = Arc::new(Submissions::new());
    let Composition { index, wallets, .. } = chains
        .compose(
            || async {
                let file = std::path::absolute(".index-data/wallet.db")?;
                tokio::task::spawn_blocking(|| std::fs::create_dir_all(".index-data")).await??;
                Ok::<_, AnyError>(Database::open(file).await?)
            },
            |store: &Database| Ok::<_, AnyError>(Repository::new(store.clone())),
            &registrar,
        )
        .await?;

    // 4. Make wallets. Only the tag says which chain a wallet is on.
    let mut signers: BTreeMap<&str, Arc<dyn Wallet>> = BTreeMap::new();
    for (asset, created, attached, seed) in WALLETS {
        let asset = asset.to_owned();
        let held = wallets
            .from_pk(&asset, SecretBytes::new(vec![seed; 32]))
            .await?;
        // A wallet is proved to belong to the asset by its factory.
        wallets.accepts(&asset, held.as_ref())?;
        for (id, wallet) in [(created, wallets.create(&asset).await?), (attached, held)] {
            // Watch first, keep the key second: a crash between the two
            // leaves a watch-only address, never a key nothing observes.
            index.watch(id, wallet.as_ref(), Birthday::Fresh).await?;
            signers.insert(id, wallet);
        }
    }

    // 5. What the index observes, and which of those this process can sign for.
    for network in index.networks() {
        for watch in index.watched(network).await? {
            let signing = signers.contains_key(watch.id.as_str());
            println!(
                "{:<14} {:<10} {} signing={signing}",
                watch.id, watch.asset.asset, watch.address.value
            );
        }
    }
    Ok(())
}

//! A shop's backend: a client signing its own sessions with its key,
//! creating a wallet, then a 15-minute token for the customer's browser
//! that can only read that wallet.
//!
//! `HASHINBOX_URL=… HASHINBOX_KEY_ID=hip/… HASHINBOX_SECRET=his/… cargo run --example backend`
//!
//! The secret never leaves this process: the client sends JWTs it signs.
//! Keep the clock synced: the server takes `iat` at most 60 s ahead.

use std::time::Duration;

use hashinbox_client::models::CreateWalletRequest;
use hashinbox_client::operations::{WalletsCreateRequest, wallets_create};
use hashinbox_client::scope::escape_id;
use hashinbox_client::{Client, Issue, Secret};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let var = |name: &str| std::env::var(name).map_err(|_| format!("{name} is not set"));
    let secret = Secret::parse(var("HASHINBOX_KEY_ID")?, var("HASHINBOX_SECRET")?)?;
    let client = Client::builder(var("HASHINBOX_URL")?)
        .secret(secret.clone())
        .timeout(Duration::from_secs(30))
        .build()?;

    let body = CreateWalletRequest {
        chain: "ethereum".into(),
        network: "mainnet".into(),
        external_id: Some("customer-1832".into()),
        address: None,
        secret: None,
    };
    let wallet = wallets_create(&client, WalletsCreateRequest { body }).await?;
    println!("wallet: {}", wallet.id);

    let scope = format!("com.hashinbox.wallets[{}]:read", escape_id(&wallet.id));
    let token = secret.issue(
        Issue::new()
            .scope(&scope)?
            .subject("customer-1832")
            .ttl(Duration::from_secs(15 * 60)),
    )?;
    println!(
        "browser token (until {}): {}",
        token.expires_at, token.token
    );
    Ok(())
}

//! A key, a token it narrows itself to, and requests checked against the
//! token: `cargo run -p scope --example token`.

use scope::{Denied, Scopes, require};

scope::schema! {
    /// What hashinbox scopes exist.
    static HASHINBOX = com.hashinbox {
        wallets => [read, write],
        wallets.legs => [read],
        transactions => [read, write],
        markets => [read],
    }
}

/// A shop's backend key: everything on wallets, prices.
const KEY: &str = "com.hashinbox.wallets.*.$* com.hashinbox.markets.$read";

/// What the shop asks the token for: its customer's two deposit wallets,
/// read only, written long-hand.
const ASKED: &str = "com.hashinbox.wallets[eth-1].$read com.hashinbox.wallets[eth-2].$read \
                     com.hashinbox.wallets[eth-1].legs.$read com.hashinbox.markets.$read";

/// A route: one wallet's balance.
fn balance(granted: &Scopes, wallet: &str) -> Result<String, Denied> {
    require!(granted, "com.hashinbox.wallets[{wallet}].$read");
    Ok(format!("{wallet}: 1.5 ETH"))
}

/// A route: a send from one wallet.
fn send(granted: &Scopes, wallet: &str) -> Result<String, Denied> {
    require!(granted, "com.hashinbox.wallets[{wallet}].$write");
    Ok(format!("{wallet}: sent"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let key: Scopes = KEY.parse()?;
    let asked: Scopes = ASKED.parse()?;

    // Each asked scope must name something, and the key must cover it.
    if let Some(unknown) = asked.iter().find(|scope| !HASHINBOX.knows(scope)) {
        return Err(format!("unknown scope {unknown}").into());
    }
    if let Some(wider) = key.first_uncovered(&asked) {
        return Err(format!("the key cannot grant {wider}").into());
    }

    // What the JWT carries.
    let token = asked.compress();
    println!("asked:  {asked}");
    println!("token:  {token}");
    println!("packed: {:?}", asked.pack());
    println!(
        "in full: {}",
        HASHINBOX.expand(&"com.hashinbox.wallets.*.$read".parse()?)
    );

    // Requests with the token.
    println!("{}", balance(&token, "eth-1")?);
    println!("{}", balance(&token, "eth-2")?);
    for refused in [balance(&token, "eth-3"), send(&token, "eth-1")] {
        println!("refused: {}", refused.unwrap_err());
    }

    // A token asking more than its key is refused.
    let greedy: Scopes = "com.hashinbox.transactions.$write".parse()?;
    println!(
        "refused token: {:?}",
        key.first_uncovered(&greedy).map(ToString::to_string)
    );
    Ok(())
}

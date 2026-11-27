//! A key, a token it narrows itself to, and requests checked against the
//! token: `cargo run -p scope --example token`.

use scope::{Denied, Scopes, require};

scope::schema! {
    /// What hashinbox scopes exist.
    static HASHINBOX = com.hashinbox {
        wallets,
        wallets.transactions,
        triggers,
        jobs,
    }
}

/// What holding a scope implies beyond its subtree.
const DEPENDS: &[(&str, &[&str])] = &[
    ("com.hashinbox.triggers", &["com.hashinbox.wallets"]),
    ("com.hashinbox.wallets", &["com.hashinbox.jobs"]),
];

/// A shop's backend key: everything on wallets.
const KEY: &str = "com.hashinbox.wallets";

/// What the shop asks the token for: its customer's two deposit wallets,
/// read only.
const ASKED: &str = "com.hashinbox.wallets[eth-1]:read com.hashinbox.wallets[eth-2]:read";

/// A route: one wallet's balance.
fn balance(granted: &Scopes, wallet: &str) -> Result<String, Denied> {
    require!(granted, "com.hashinbox.wallets[{wallet}]:read");
    Ok(format!("{wallet}: 1.5 ETH"))
}

/// A route: a send from one wallet.
fn send(granted: &Scopes, wallet: &str) -> Result<String, Denied> {
    require!(granted, "com.hashinbox.wallets[{wallet}].transactions");
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

    // What the JWT carries, and what it allows once expanded.
    let token = asked.compress();
    println!("token:    {token}");
    println!("packed:   {:?}", token.pack());
    println!("expanded: {}", token.expand(DEPENDS));

    // Requests with the token.
    let granted = token.expand(DEPENDS);
    println!("{}", balance(&granted, "eth-1")?);
    for refused in [balance(&granted, "eth-3"), send(&granted, "eth-1")] {
        println!("refused: {}", refused.unwrap_err());
    }

    // A token asking more than its key is refused.
    let greedy: Scopes = "com.hashinbox.triggers".parse()?;
    println!(
        "refused token: {:?}",
        key.first_uncovered(&greedy).map(ToString::to_string)
    );
    Ok(())
}

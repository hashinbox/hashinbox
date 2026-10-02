# wallets

Create wallets, import keys and send payments on any chain without naming a
chain crate. `Wallets` is a map from your own asset tags to the wallet factory
each chain provides, and `Composition` assembles the configured chains into one
index and one wallet map. Balance and history reads are `indexing`'s.

## Usage

```rust
use indexing::Holder;
use wallets::{SecretBytes, Transfer, WalletFactory, Wallets};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `factory` comes from a chain crate. Nothing below names a chain.
async fn run(factory: impl WalletFactory + 'static, key: SecretBytes) -> Result<()> {
    let mut wallets = Wallets::<&str>::new();
    wallets.add("coin", factory)?;

    // The map makes wallets and keeps none of them.
    let treasury = wallets.from(&"coin", key).await?;
    let alice = wallets.create(&"coin").await?;
    let to = wallets.parse(&"coin", &alice.address().value)?;

    // One transfer.
    let author = wallets.author(&"coin", &treasury)?;
    let id = author.send(to.clone(), "0.5".parse()?).await?;

    // An ordered batch.
    let transfer = Transfer { wallet: treasury, to, amount: "0.1".parse()? };
    let ids = wallets.send_all(&"coin", vec![transfer]).await?;

    println!("{id} {ids:?}");
    Ok(())
}
```

## Notes

- `Wallets` stores no wallet and no key. Register a new wallet's address with
  the index (`watch`) before you store its key: a crash in between then leaves
  an address that is watched without a key, never a key whose deposits nobody
  observes.
- A failed batch is not rolled back. `SendError::accepted` lists the
  transactions a node already accepted, so resending the whole batch pays those
  transfers twice.
- An accepted send means submitted, not confirmed. Confirmation comes from
  indexing.

Transfers accept `Amount::Exact(decimal)` or `Amount::All`. A sweep resolves
inside the chain's existing send reservation; a batch may contain one sweep
per source, after that source's fixed transfers. It spends available funds
after fees, without using incoming transfers in the same batch as funding.
Ethereum budgets maximum EIP-1559 fees, so unused fees can leave a residue.
Draft builders continue to describe exact amounts.

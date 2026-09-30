# wallets

Create wallets, import keys and send payments on any chain without naming a
chain crate. `Wallets` is a map from your own asset tags to the wallet factory
each chain provides, and `Composition` assembles the configured chains into one
index, one wallet map and the balance and history readers.

## Usage

```rust
use wallets::{Holder, SecretBytes, Transfer, WalletFactory, Wallets};

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

Balances and history are read through `PendingJoin`, built once per network
from the repository and the index that commits into it:

```rust
use std::sync::Arc;

use indexing::{BalanceStore, ConfirmedHistory, Pending};
use wallets::{Balances, History, Holder, Page, PendingJoin};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `index` is the index that commits into `repository`, for the same network.
async fn read<R>(repository: R, index: Arc<dyn Pending>, of: &dyn Holder) -> Result<()>
where
    R: ConfirmedHistory + BalanceStore,
{
    let reads = PendingJoin::new(repository, index);

    let balance = reads.balance(of).await?;
    let page = reads.history(of, Page::first(25)).await?;

    println!("{} settled, {} arriving", balance.amount, balance.pending.incoming);
    println!("{} confirmed, {} pending", page.transactions.len(), page.pending.len());
    println!("{:?}", balance.completeness);
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
- `History` and `Balances` are the two pending-inclusive reads, and
  `PendingJoin` answers both, and the spendable set, for one network. It joins a
  repository's own answer to the blocks that network's index still holds above
  its confirmed line. The index answers no query: it hands out a sample of
  those blocks (`indexing::Pending`). The join samples them **before** it reads
  the repository and narrows the sample by the checkpoint the repository's
  answer came with, so a block committed during the read is counted once.
- How much of an address's history is indexed is the repository's claim
  (`indexing::Completeness`), made from the rows it holds. `PendingJoin` passes
  it on unchanged, and a repository's refusal to list outputs it cannot prove
  unspent reaches the caller as the repository made it.

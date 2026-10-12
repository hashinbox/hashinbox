# wallets

Create wallets, import keys and send payments on any chain without naming a
chain crate. `Wallets` is a map from your own asset tags to the wallet factory
each chain provides. Its core depends on `base` and `crypto`; `payments::config::Composition`
assembles configured chains into one index and one wallet map.

## Usage

```rust
use base::Holder;
use wallets::{SecretBytes, Transfer, WalletFactory, Wallets};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// `factory` comes from a chain crate. Nothing below names a chain.
async fn run(factory: impl WalletFactory + 'static, key: SecretBytes) -> Result<()> {
    let mut wallets = Wallets::<&str>::new();
    wallets.add("coin", factory)?;

    // The map makes wallets and keeps none of them.
    let treasury = wallets.from_pk(&"coin", key).await?;
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

- Feature `submissions` adds `Submissions`, a wallet registrar over `tasks::Tasks`;
  close and drain it before stopping indexing during shutdown.

- `Wallets` persists no wallet and no key. It holds live signers in memory:
  `load_signers` rebuilds them from a `WalletRepository` for served networks,
  `insert_signer`/`remove_signers`/`signer` manage them. Register a new wallet's address with
  the index (`watch`) before you store its key: a crash in between then leaves
  an address that is watched without a key, never a key whose deposits nobody
  observes.
- A failed batch is not rolled back. `SendError::accepted` lists the
  transactions a node already accepted, so resending the whole batch pays those
  transfers twice.
- Feature `testing` (off by default, dev-dependency only) adds `wallets::testing`:
  one scripted factory with an `Outbox` that records every send and answers with a
  chosen `Outcome`. Use it instead of writing another wallet double.
- `Wallets::queue(&Policy)` puts every sending asset behind an in-memory
  `Queue`: FIFO, bounded in flight per network, retried only when nothing was
  accepted. Pass a `Pause` (e.g. `tokio::time::sleep`); `queues()` reads its
  `Stats`, and `close()` refuses new sends while admitted ones finish.
- `Wallets::pause(&network)` refuses every later queued send of that network's
  assets and every `Wallets::admit` (non-send work an application counts, such as
  registering a watch) with `WalletErrorKind::Paused`; `Suspension::drained()` waits for
  those already admitted, and dropping the `Suspension` admits again. It is the
  application's half of an online rescan.
- An accepted send means submitted, not confirmed. Confirmation comes from
  the index.

Transfers accept `Amount::Exact(decimal)` or `Amount::All`. A sweep resolves
inside the chain's existing send reservation; a batch may contain one sweep
per source, after that source's fixed transfers. It spends available funds
after fees, without using incoming transfers in the same batch as funding.
Ethereum budgets maximum EIP-1559 fees, so unused fees can leave a residue.
Draft builders continue to describe exact amounts.

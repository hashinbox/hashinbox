# 092: The funds check counts only what a transaction pays

Status: proposed (doc only). It fixes `docs/issues.md` item 5.

## Problem

- The funds check in `transaction::db::save_pending` (`apps/server/src/models/transaction.rs`) sums
  each wallet index's legs, both outgoing and incoming.
- A self-send of 0.03 ETH has two legs on the sender's index:
  - out: −0.03 − fee;
  - in: +0.03.
- They net to −fee, so the check asks only whether the fee is spendable.
- The balance view (`balance_confirmed_spendable` in `init.sql`) subtracts outgoing pending legs
  and ignores incoming ones, so the check and the view disagree.
- **Seen live (QA 2026-11-23):**
  - two 0.03 ETH self-sends with 0.0484 ETH spendable were both accepted;
  - spendable read −0.0116 ETH for about 3 minutes;
  - every other send from that wallet was refused meanwhile.
- **Risk:** on Ethereum a transaction needs its full value plus fee when it runs; what comes back
  arrives only after it ran. If another spend takes the balance below that, the self-send fails at
  its nonce, and every later nonce from that wallet waits behind it until a cancel lands.

## Rule

- A transaction pays from an index what that index's **outgoing** legs take (value and fee).
- An incoming leg on the same index pays nothing: it arrives only once the transaction has run.
- This is the rule the spendable view already uses, so the check and the view agree.

## Change

In `save_pending`, build `takes` from outgoing legs only. Keep an entry for every index the
transaction touches: the same keys feed `find_spending`, and its result decides which legs are kept
(`transaction.keep(&ours)`), so a receiver must stay in the map.

```rust
// What each index pays: its outgoing legs only (value and fee). An
// incoming leg on the same index (a self-send) arrives only once the
// transaction has run, so it cannot pay for it.
let mut takes = BTreeMap::<String, Decimal>::new();
for leg in &transaction.legs {
    let take = takes
        .entry(leg.wallet_index_id.clone().into())
        .or_insert_with(Decimal::zero);
    let signed = leg.signed()?;
    if signed < Decimal::zero() {
        *take += signed;
    }
}
```

The rest of `save_pending` stays the same:
- a pure receiver sums to 0 and is skipped by `if *takes >= Decimal::zero() { continue; }`;
- `can_spend` compares against spendable minus what is in flight.

## Effect

- **A self-send of 0.03 with 0.0484 spendable** needs 0.03 + fee: accepted.
- **A second one:** 0.0484 − (0.03 + fee) is left, less than 0.03 + fee: refused as short
  (`insufficient-funds`).
- **A send to another wallet:** unchanged. It has no incoming leg on the sender's index.
- **A send to a wallet this server also indexes:** unchanged for the receiver. Its index is still
  looked up and its incoming pending leg still kept.

## Test

In `apps/server/tests/check.rs` style:
- a wallet with 0.0484 sends two self-sends of 0.03;
- the first is recorded and the second refused as short;
- `balance_confirmed_spendable` never goes below 0.

//! Values of base types tests build chains and stores from.

use crate::{
    Address, Asset, Block, BlockHash, BlockHeader, BlockParent, Blocks, Coin, Decimal, Leg,
    Network, Transaction, TransactionId, TransactionState,
};

/// Header `position` of branch `fork`; the parent is `position - 1` of the
/// same branch. Its hash: the position's bytes from the first nonzero one,
/// then the fork.
#[must_use]
pub fn header(position: u64, fork: u8) -> BlockHeader {
    let hash = |position: u64| {
        let bytes = position.to_be_bytes();
        let from = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
        BlockHash([&bytes[from..], &[fork]].concat())
    };
    BlockHeader {
        position,
        height: position,
        hash: hash(position),
        parent: position.checked_sub(1).map(|parent| BlockParent {
            position: parent,
            hash: hash(parent),
        }),
        timestamp: None,
    }
}

/// Block [`header`]`(position, fork)` holding `transactions`.
#[must_use]
pub fn block(network: &Network, position: u64, fork: u8, transactions: Vec<Transaction>) -> Block {
    Block {
        network: network.clone(),
        header: header(position, fork),
        transactions: transactions.into(),
        rewards: Vec::new(),
    }
}

/// `blocks` as one linked run.
///
/// # Panics
/// When a block does not link onto the one before it.
#[must_use]
pub fn run(network: &Network, blocks: Vec<Block>) -> Blocks {
    let mut run = Blocks::new(network.clone());
    for block in blocks {
        assert!(run.push(block));
    }
    run
}

/// Empty blocks `0..=tip` of branch 0.
#[must_use]
pub fn quiet(network: &Network, tip: u64) -> Vec<Block> {
    (0..=tip).map(|p| block(network, p, 0, vec![])).collect()
}

/// Output `index` of `txid`.
#[must_use]
pub fn coin(network: &Network, txid: &str, index: u32) -> Coin {
    Coin {
        transaction: TransactionId::new(network.clone(), txid),
        index,
    }
}

/// A transaction `txid` paying `amount` of the native asset to `to` (coin
/// `txid:0`) and, when given, spending `spends` from `from`.
#[must_use]
pub fn payment(
    network: &Network,
    txid: &str,
    to: &str,
    amount: u64,
    spends: Option<(&str, Coin)>,
) -> Transaction {
    let native = Asset::native(network.clone());
    let mut legs = vec![Leg::Received {
        address: Address::new(network.clone(), to),
        asset: native.clone(),
        amount: Decimal::from(amount),
        coin: Some(coin(network, txid, 0)),
    }];
    if let Some((from, spent)) = spends {
        legs.push(Leg::Spent {
            address: Address::new(network.clone(), from),
            asset: native,
            amount: Decimal::from(amount),
            coin: Some(spent),
        });
    }
    Transaction {
        id: TransactionId::new(network.clone(), txid),
        state: TransactionState::Succeeded,
        order: Some(0),
        legs,
        bytes: txid.as_bytes().to_vec(),
        expires_height: None,
    }
}

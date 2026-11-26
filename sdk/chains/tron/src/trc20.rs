//! TRC-20 (Ethereum's ERC-20 ABI): the `transfer` call a wallet makes and
//! the `Transfer` event it indexes.

use alloy_primitives::{hex, keccak256};
use num_bigint::BigUint;

use crate::Account;

/// `transfer(address,uint256)`'s selector.
const TRANSFER_CALL: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// `transfer(to, amount)` calldata: the selector, `to`'s word and the
/// 32-byte big-endian `amount`.
pub(crate) fn transfer(to: &Account, amount: [u8; 32]) -> Vec<u8> {
    let mut data = TRANSFER_CALL.to_vec();
    data.extend(to.word());
    data.extend(amount);
    data
}

/// `(from, to, amount)` of a log of `topics` (hex words) and `data` (hex),
/// when it is a `Transfer`; the zero account is a mint's `from` and a
/// burn's `to`.
pub(crate) fn transfer_log(topics: &[String], data: &str) -> Option<(Account, Account, BigUint)> {
    let [signature, from, to] = topics else {
        return None;
    };
    let event = keccak256("Transfer(address,address,uint256)");
    let data = hex::decode(data).ok()?;
    if hex::decode(signature).ok()? != event.as_slice() || data.len() != 32 {
        return None;
    }
    Some((word(from)?, word(to)?, BigUint::from_bytes_be(&data)))
}

/// The account of a topic: a 32-byte word, the 20 bytes left-padded.
fn word(topic: &str) -> Option<Account> {
    let bytes = hex::decode(topic).ok()?;
    let (padding, account) = bytes.split_at_checked(12)?;
    if padding.iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(Account(account.try_into().ok()?))
}

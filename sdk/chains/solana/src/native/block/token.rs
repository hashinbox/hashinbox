use std::collections::BTreeMap;

use super::wire::TokenWire;
use crate::native::{Address, Error, Lamport, spl::TokenTransfer};

/// What identifies one position inside a transaction: the account key it
/// occupies, its owner, and its mint.
type Held = (usize, Address, Address);

/// The units one position held on each side of execution; a side the
/// transaction does not report is absent, not zero, until the two arrays are
/// merged.
type Units = (Option<Lamport>, Option<Lamport>);

/// One account's SPL token position inside a transaction: whose it is, which
/// mint it holds, and the units it held around execution.
///
/// The runtime's own `preTokenBalances` / `postTokenBalances` are the
/// authority here, exactly as `preBalances` / `postBalances` are for lamports.
/// A side the transaction does not report is zero, not an error: the token
/// account was created or closed by this transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenBalance {
    /// Index of the token account in the transaction's account keys.
    pub account: usize,
    /// The wallet the token account belongs to; movements name it, not the
    /// token account, so no associated-token address has to be tracked.
    pub owner: Address,
    /// The mint this position holds.
    pub mint: Address,
    /// Units before execution.
    pub before: Lamport,
    /// Units after execution.
    pub after: Lamport,
}

impl TokenBalance {
    /// Every position of one transaction, in account-key order.
    ///
    /// An absent array (the node recorded no token balances) yields no
    /// positions, and an entry without an owner or without a plain unit count
    /// — a Token-2022 confidential balance, say — is left out: it names no
    /// wallet to attribute, and leaving it out costs attribution rather than
    /// halting the chain.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] for an entry naming an
    /// account key the transaction does not resolve, a malformed mint or
    /// owner, or the same position twice.
    pub(super) fn parse(
        pre: Option<Vec<TokenWire>>,
        post: Option<Vec<TokenWire>>,
        keys: usize,
    ) -> Result<Vec<Self>, Error> {
        let mut positions = BTreeMap::new();
        for (side, entries) in [(Side::Before, pre), (Side::After, post)] {
            Self::merge(&mut positions, side, entries.unwrap_or_default(), keys)?;
        }
        Ok(positions
            .into_iter()
            .map(|((account, owner, mint), (before, after))| Self {
                account,
                owner,
                mint,
                before: before.unwrap_or(Lamport::ZERO),
                after: after.unwrap_or(Lamport::ZERO),
            })
            .collect())
    }

    /// The single position `index` names, if the transaction reports exactly
    /// one there.
    pub(crate) fn at(balances: &[Self], index: usize) -> Option<&Self> {
        let mut found = balances.iter().filter(|balance| balance.account == index);
        found.next().filter(|_| found.next().is_none())
    }

    /// Units this position moved that `transfers` do not attribute: a credit
    /// when positive, a debit when negative.
    pub(super) fn unexplained(&self, transfers: &[TokenTransfer]) -> i128 {
        let endpoint = |account: usize| i128::from(account == self.account);
        let attributed: i128 = transfers
            .iter()
            .map(|transfer| {
                let net = endpoint(transfer.destination) - endpoint(transfer.source);
                net * i128::from(transfer.amount.units())
            })
            .sum();
        i128::from(self.after.units()) - i128::from(self.before.units()) - attributed
    }

    /// Whether this position belongs to `owner` in `mint`.
    pub(super) fn held_by(&self, owner: &Address, mint: &Address) -> bool {
        self.owner == *owner && self.mint == *mint
    }

    /// The net units this position gained (positive) or lost (negative).
    pub(super) fn delta(&self) -> i128 {
        i128::from(self.after.units()) - i128::from(self.before.units())
    }

    fn merge(
        positions: &mut BTreeMap<Held, Units>,
        side: Side,
        entries: Vec<TokenWire>,
        keys: usize,
    ) -> Result<(), Error> {
        for entry in entries {
            let Some((key, units)) = entry.position(keys)? else {
                continue;
            };
            let amounts = positions.entry(key).or_insert((None, None));
            let recorded = match side {
                Side::Before => &mut amounts.0,
                Side::After => &mut amounts.1,
            };
            if recorded.replace(units).is_some() {
                return Err(Error::invalid_block(
                    "Solana transaction reports one token position twice",
                ));
            }
        }
        Ok(())
    }
}

/// Which of a transaction's two token-balance arrays an entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Before,
    After,
}

impl TokenWire {
    /// The position and units this entry records; `None` when it names no
    /// owner or no plain unit count.
    fn position(self, keys: usize) -> Result<Option<(Held, Lamport)>, Error> {
        let account = usize::from(self.account_index);
        if account >= keys {
            return Err(Error::invalid_block(
                "Solana token balance names an unresolved account key",
            ));
        }
        let mint = Self::address(&self.mint)?;
        let (Some(owner), Some(units)) = (self.owner, self.amount.units) else {
            return Ok(None);
        };
        Ok(Some((
            (account, Self::address(&owner)?, mint),
            Lamport::from_atomic(units),
        )))
    }

    fn address(text: &str) -> Result<Address, Error> {
        text.parse::<Address>().map_err(|_| {
            Error::invalid_block("Solana token balance contains a malformed canonical address")
        })
    }
}

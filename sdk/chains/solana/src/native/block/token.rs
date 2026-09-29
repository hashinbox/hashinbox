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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn address(byte: u8) -> String {
        Address::from_bytes([byte; 32]).to_string()
    }

    fn entry(index: u8, owner: Option<&str>, amount: serde_json::Value) -> TokenWire {
        let mut value = json!({
            "accountIndex": index,
            "mint": address(9),
            "programId": address(1),
            "uiTokenAmount": {"decimals": 6, "uiAmountString": "1", "amount": amount},
        });
        if let Some(owner) = owner {
            value["owner"] = json!(owner);
        }
        serde_json::from_value(value).expect("fixture token balance shape")
    }

    fn parse(pre: Vec<TokenWire>, post: Vec<TokenWire>) -> Vec<TokenBalance> {
        TokenBalance::parse(Some(pre), Some(post), 4).expect("fixture positions")
    }

    #[test]
    fn a_missing_side_is_zero_because_the_account_was_created_or_closed() {
        let owner = address(2);
        let created = parse(Vec::new(), vec![entry(1, Some(&owner), json!("500"))]);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].before, Lamport::ZERO);
        assert_eq!(created[0].after.units(), 500);
        assert_eq!(created[0].delta(), 500);

        let closed = parse(vec![entry(1, Some(&owner), json!("500"))], Vec::new());
        assert_eq!(closed[0].after, Lamport::ZERO);
        assert_eq!(closed[0].delta(), -500);
    }

    #[test]
    fn absent_arrays_and_unattributable_entries_leave_the_chain_indexing() {
        assert!(TokenBalance::parse(None, None, 4).unwrap().is_empty());
        let owner = address(2);
        assert!(
            TokenBalance::parse(None, Some(vec![entry(1, None, json!("5"))]), 4)
                .unwrap()
                .is_empty(),
            "an entry without an owner names no wallet"
        );
        for amount in [json!(null), json!("not a number"), json!(5)] {
            assert!(
                TokenBalance::parse(None, Some(vec![entry(1, Some(&owner), amount)]), 4)
                    .unwrap()
                    .is_empty(),
                "a balance without a plain unit count must not halt indexing"
            );
        }
    }

    #[test]
    fn positions_key_on_account_owner_and_mint_in_account_key_order() {
        let (first, second) = (address(2), address(3));
        let positions = parse(
            vec![
                entry(2, Some(&second), json!("7")),
                entry(1, Some(&first), json!("500")),
            ],
            vec![
                entry(1, Some(&first), json!("300")),
                entry(2, Some(&second), json!("207")),
            ],
        );
        assert_eq!(
            positions
                .iter()
                .map(|position| (position.account, position.delta()))
                .collect::<Vec<_>>(),
            vec![(1, -200), (2, 200)]
        );
        assert!(positions[0].held_by(&first.parse().unwrap(), &address(9).parse().unwrap()));
        assert!(!positions[0].held_by(&second.parse().unwrap(), &address(9).parse().unwrap()));
        assert_eq!(TokenBalance::at(&positions, 1), Some(&positions[0]));
        assert_eq!(TokenBalance::at(&positions, 3), None);
    }

    #[test]
    fn one_account_reinitialized_into_another_position_stays_two_positions() {
        let owner = address(2);
        let mut closed = entry(1, Some(&owner), json!("500"));
        closed.mint = address(8);
        let positions = parse(vec![closed], vec![entry(1, Some(&owner), json!("9"))]);
        assert_eq!(positions.len(), 2);
        assert_eq!(TokenBalance::at(&positions, 1), None);
        assert_eq!(
            positions
                .iter()
                .map(TokenBalance::delta)
                .collect::<Vec<_>>(),
            vec![-500, 9]
        );
    }

    #[test]
    fn structural_violations_reject_the_block() {
        let owner = address(2);
        let mut unknown = entry(1, Some(&owner), json!("5"));
        unknown.account_index = 9;
        assert_eq!(
            TokenBalance::parse(Some(vec![unknown]), None, 4)
                .unwrap_err()
                .message,
            "Solana token balance names an unresolved account key"
        );

        let mut malformed = entry(1, Some(&owner), json!("5"));
        malformed.mint = "not-an-address".to_owned();
        assert_eq!(
            TokenBalance::parse(Some(vec![malformed]), None, 4)
                .unwrap_err()
                .message,
            "Solana token balance contains a malformed canonical address"
        );

        let repeated = vec![
            entry(1, Some(&owner), json!("5")),
            entry(1, Some(&owner), json!("6")),
        ];
        assert_eq!(
            TokenBalance::parse(Some(repeated), None, 4)
                .unwrap_err()
                .message,
            "Solana transaction reports one token position twice"
        );
    }

    #[test]
    fn unexplained_units_are_the_delta_the_decoded_transfers_leave() {
        let owner = address(2);
        let positions = parse(
            vec![entry(1, Some(&owner), json!("500"))],
            vec![entry(1, Some(&owner), json!("300"))],
        );
        let attributed = TokenTransfer {
            source: 1,
            destination: 2,
            amount: Lamport::from_atomic(150),
        };
        assert_eq!(positions[0].unexplained(&[]), -200);
        assert_eq!(positions[0].unexplained(&[attributed]), -50);
        assert_eq!(
            positions[0].unexplained(&[attributed, attributed]),
            100,
            "over-attribution is a credit, never a silent zero"
        );
    }
}

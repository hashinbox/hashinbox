//! What indexed facts do to one holding: a signed amount in the asset's
//! **atomic** units — positive a credit, negative a debit, zero nothing.
//!
//! This crate never learns an asset's decimals, so every amount here is exact
//! at scale 0 and the arithmetic is independent of them. A deficit stays
//! negative: building a balance from a net, and saying what a negative one
//! means, is the reader's business.

use num_bigint::BigInt;

use crate::{
    Address, BlockCredit, Decimal, FeeSettlement, Holder, IndexError, IndexErrorKind, Transaction,
    Transactions, ValueMovement,
};

impl ValueMovement {
    /// What this movement does to `address`: its amount to the recipient, its
    /// negation to the sender, and zero to a self-transfer or a stranger.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when the amount is not a non-negative
    /// integer, which the write path forbids.
    pub fn effect(&self, address: &Address) -> Result<BigInt, IndexError> {
        let endpoint = |value: Option<&Address>| value == Some(address);
        match (endpoint(self.to()), endpoint(self.from())) {
            (true, false) => units(&self.amount),
            (false, true) => units(&self.amount).map(|amount| -amount),
            (true, true) | (false, false) => Ok(BigInt::ZERO),
        }
    }
}

impl Transaction {
    /// What this transaction does to `of`: its movements in `of`'s asset, and
    /// the fee `of` paid when `settlement` charges it beside the movements and
    /// in `of`'s asset.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when an amount is not a non-negative integer.
    pub fn effect(&self, of: &dyn Holder, settlement: FeeSettlement) -> Result<BigInt, IndexError> {
        let mut net = BigInt::ZERO;
        for movement in self
            .movements
            .iter()
            .filter(|movement| &movement.asset == of.asset())
        {
            net += movement.effect(of.address())?;
        }
        let charged = self.fee.as_ref().filter(|fee| {
            settlement == FeeSettlement::Charged
                && fee.payer.as_ref() == Some(of.address())
                && &fee.asset == of.asset()
        });
        if let Some(fee) = charged {
            net -= units(&fee.amount)?;
        }
        Ok(net)
    }
}

impl BlockCredit {
    /// What this transactionless credit does to `of`: its amount when it
    /// names `of`'s address and asset, zero otherwise.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when the amount is not a non-negative integer.
    pub fn effect(&self, of: &dyn Holder) -> Result<BigInt, IndexError> {
        if &self.asset == of.asset() && &self.to == of.address() {
            return units(&self.amount);
        }
        Ok(BigInt::ZERO)
    }
}

/// The net of a collection of transactions to one holding.
///
/// A trait only because [`Transactions`] is `base`'s collection, which this
/// crate cannot extend inherently; the computation is this crate's.
pub trait Net {
    /// Every transaction's [`Transaction::effect`] on `of`, summed exactly.
    ///
    /// # Errors
    ///
    /// [`IndexErrorKind::Store`] when an amount is not a non-negative integer.
    fn net(&self, of: &dyn Holder, settlement: FeeSettlement) -> Result<BigInt, IndexError>;
}

impl Net for Transactions<Transaction> {
    fn net(&self, of: &dyn Holder, settlement: FeeSettlement) -> Result<BigInt, IndexError> {
        self.iter().try_fold(BigInt::ZERO, |net, transaction| {
            Ok(net + transaction.effect(of, settlement)?)
        })
    }
}

/// One stored amount as atomic units.
fn units(amount: &Decimal) -> Result<BigInt, IndexError> {
    amount.to_atomic(0).map(BigInt::from).map_err(|error| {
        IndexError::new(
            IndexErrorKind::Store,
            format!("stored amount is not a non-negative integer: {error}"),
            false,
        )
    })
}

#[cfg(test)]
mod tests {
    use base::ChainName;

    use super::*;
    use crate::{
        AssetId, BlockHash, BlockHeight, BlockPosition, BlockRef, MovementId, Network, NetworkFee,
        Status, Subject, TransactionRef,
    };

    fn network() -> Network {
        Network::new(ChainName::new("test"), "mainnet")
    }

    fn native() -> AssetId {
        AssetId::native(network().chain)
    }

    fn subject(value: &str) -> Subject {
        Subject {
            address: Address::new(network(), value),
            asset: native(),
        }
    }

    fn block() -> BlockRef {
        BlockRef {
            position: BlockPosition(1),
            height: BlockHeight(1),
            hash: BlockHash(vec![1]),
            parent: None,
            timestamp: None,
        }
    }

    /// A transfer of `amount` to `to`, with `payer` charged a fee of 3.
    fn transaction(id: &str, to: &str, amount: u64, payer: &str) -> Transaction {
        Transaction {
            id: TransactionRef::new(network(), id),
            movements: vec![ValueMovement::transfer(
                MovementId("value".into()),
                native(),
                Decimal::from(amount),
                Address::new(network(), "sender"),
                Address::new(network(), to),
            )],
            fee: Some(NetworkFee {
                asset: native(),
                amount: Decimal::from(3_u64),
                payer: Some(Address::new(network(), payer)),
            }),
            order: 0,
            status: Status::Included { block: block() },
        }
    }

    fn credit(id: &str, to: &str, asset: AssetId, amount: u64) -> BlockCredit {
        BlockCredit {
            id: MovementId(id.into()),
            asset,
            amount: Decimal::from(amount),
            to: Address::new(network(), to),
        }
    }

    /// The recipient gains, the sender loses the amount and the fee, a
    /// stranger is untouched, and a settled fee is not charged twice.
    #[test]
    fn a_movement_credits_its_recipient_and_debits_its_sender_and_fee_payer() {
        let paid = transaction("a", "recipient", 40, "sender");
        let effect = |of: &str, settlement| paid.effect(&subject(of), settlement).expect("atomic");
        assert_eq!(
            effect("recipient", FeeSettlement::Charged),
            BigInt::from(40)
        );
        assert_eq!(effect("sender", FeeSettlement::Charged), BigInt::from(-43));
        assert_eq!(effect("sender", FeeSettlement::Embedded), BigInt::from(-40));
        assert_eq!(effect("stranger", FeeSettlement::Charged), BigInt::ZERO);
    }

    /// A collection nets every transaction, and a deficit stays negative.
    #[test]
    fn transactions_net_every_effect_and_keep_a_deficit() {
        let held = Transactions::new(vec![
            transaction("a", "owner", 10, "sender"),
            transaction("b", "elsewhere", 0, "owner"),
            transaction("c", "elsewhere", 0, "owner"),
            transaction("d", "elsewhere", 0, "owner"),
            transaction("e", "elsewhere", 0, "owner"),
        ])
        .expect("unique ids");
        assert_eq!(
            held.net(&subject("owner"), FeeSettlement::Charged)
                .expect("atomic"),
            BigInt::from(-2)
        );
    }

    /// A credit names an asset and an address, and reaches only that holding.
    #[test]
    fn a_credit_reaches_only_the_holding_it_names() {
        let token = AssetId {
            chain: network().chain,
            asset: "token".into(),
        };
        let given = credit("withdrawal:1", "beneficiary", native(), 9);
        assert_eq!(
            given.effect(&subject("beneficiary")).expect("atomic"),
            BigInt::from(9)
        );
        assert_eq!(
            given.effect(&subject("stranger")).expect("atomic"),
            BigInt::ZERO
        );
        let token_subject = Subject {
            address: Address::new(network(), "beneficiary"),
            asset: token,
        };
        assert_eq!(given.effect(&token_subject).expect("atomic"), BigInt::ZERO);
    }
}

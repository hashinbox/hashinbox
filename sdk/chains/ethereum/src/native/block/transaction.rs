use std::collections::BTreeSet;

use base::{Identified, MovementKind};

use super::{Receipt, Trace, head::Sent};
use crate::native::{Address, AssetKind, Error, TransactionId, Wei};

/// One parsed transaction as included in a block, with its receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transaction {
    id: TransactionId,
    fee: Fee,
    residuals: Vec<Movement>,
    /// The sender, who pays the fee.
    pub from: Address,
    /// `None` for contract creation.
    pub to: Option<Address>,
    /// Ether attached to the transaction.
    pub value: Wei,
    /// Position in the block.
    pub index: u64,
    /// Execution result: status, gas, created contract, logs.
    pub receipt: Receipt,
    /// Every ether balance this transaction changed, as the node executed it.
    pub trace: Trace,
}

/// The network fee paid by one transaction: `gas_used * effective_gas_price`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fee {
    /// Wei paid.
    pub amount: Wei,
    /// The account charged; the sender on Ethereum.
    pub payer: Option<Address>,
}

/// One value change inside a [`Transaction`], in native types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Movement {
    /// Stable within the block: `<txid>:value`, `<txid>:<log index>`, or
    /// `<txid>:residual:<address>` for a residual.
    pub id: String,
    /// [`MovementKind::Transfer`] names both endpoints; a residual names one
    /// account only, so it credits it as a [`MovementKind::Mint`] and debits
    /// it as a [`MovementKind::Burn`].
    pub kind: MovementKind,
    /// Ether or the ERC-20 token contract.
    pub asset: AssetKind,
    /// Atomic units of `asset`.
    pub amount: Wei,
    /// Debited account; `None` for a mint or a residual credit.
    pub from: Option<Address>,
    /// Credited account; `None` for a burn or a residual debit.
    pub to: Option<Address>,
}

impl Transaction {
    pub(super) fn new(sent: Sent, receipt: Receipt, trace: Trace) -> Result<Self, Error> {
        let amount = receipt
            .effective_gas_price
            .checked_mul_u64(receipt.gas_used)
            .ok_or_else(|| {
                Error::invalid_block("Ethereum receipt fee exceeds 256-bit atomic units")
            })?;
        let mut transaction = Self {
            fee: Fee {
                amount,
                payer: Some(sent.from.clone()),
            },
            residuals: Vec::new(),
            id: sent.id,
            from: sent.from,
            to: sent.to,
            value: Wei::from_u256(sent.value),
            index: sent.index,
            receipt,
            trace,
        };
        transaction.residuals = transaction.residuals()?;
        Ok(transaction)
    }

    /// The transaction hash.
    #[must_use]
    pub const fn id(&self) -> &TransactionId {
        &self.id
    }

    /// Whether the receipt status is 1.
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        self.receipt.succeeded
    }

    /// Always present on Ethereum: failed transactions pay too.
    #[must_use]
    pub const fn fee(&self) -> Option<&Fee> {
        Some(&self.fee)
    }

    /// Accounts this transaction touches directly: sender, recipient, created contract.
    pub fn parties(&self) -> impl Iterator<Item = &Address> {
        [
            Some(&self.from),
            self.to.as_ref(),
            self.receipt.contract_address.as_ref(),
        ]
        .into_iter()
        .flatten()
    }

    /// The ether transfer, every ERC-20 `Transfer` log, then every residual.
    ///
    /// A failed transaction executed nothing, so it names no transfer and
    /// emits no log — but it is **not** movement-free: gas still settles, so
    /// its residuals stand and the block's beneficiary is still credited the
    /// priority fee it earned. Dropping them understates a watched
    /// fee-recipient address for good, because the balance is folded from
    /// these movements and nothing else can correct it.
    ///
    /// Mints (`from` zero) and burns (`to` zero) keep one endpoint only.
    /// Structurally malformed transfer logs are skipped, not errors.
    #[must_use]
    pub fn movements(&self) -> Vec<Movement> {
        if !self.succeeded() {
            return self.residuals.clone();
        }
        let tokens = self
            .receipt
            .logs
            .iter()
            .filter_map(|log| log.movement(&self.id));
        self.value()
            .into_iter()
            .chain(tokens)
            .chain(self.residuals.iter().cloned())
            .collect()
    }

    /// The top-level `value` this transaction carried, as a movement.
    ///
    /// This is the only ether movement the transaction's own fields name, and
    /// it stays the only one derived from them: the trace confirms it rather
    /// than restating it, or every plain transfer would count twice.
    fn value(&self) -> Option<Movement> {
        self.to
            .as_ref()
            .or(self.receipt.contract_address.as_ref())
            .filter(|_| !self.value.is_zero())
            .map(|to| Movement {
                id: format!("{}:value", self.id),
                kind: MovementKind::Transfer,
                asset: AssetKind::Native,
                amount: self.value.clone(),
                from: Some(self.from.clone()),
                to: Some(to.clone()),
            })
    }

    /// One movement per account whose ether the top-level `value` and the fee
    /// it paid do not account for, in address order.
    ///
    /// Contract-initiated ether — a router forwarding a payment, an escrow
    /// releasing one, a `SELFDESTRUCT` beneficiary credit, the value leg of
    /// any `CALL` — reaches an account without a counterpart the block names,
    /// so it is recorded one-sided rather than dropped. A plain transfer
    /// leaves nothing over and produces none of these.
    ///
    /// An account the `value` movement names is settled even when the trace
    /// omits it: a contract that forwards on everything it was sent ends the
    /// transaction where it started, so only the debit that cancels the
    /// attributed credit makes its balance come out right.
    ///
    /// A failed transaction attributes nothing, because nothing executed — but
    /// its trace is still read. A revert undoes the execution, never the fee,
    /// so the balances the diff can still report are exactly the settlement:
    /// the sender's gas charge, which the fee below cancels, and the
    /// beneficiary's priority fee, which is a real credit with no other record.
    fn residuals(&self) -> Result<Vec<Movement>, Error> {
        let attributed = if self.succeeded() { self.value() } else { None };
        let endpoints = attributed.iter().flat_map(Movement::endpoints);
        let accounts: BTreeSet<&Address> = self
            .trace
            .changes()
            .map(|(account, _)| account)
            .chain(endpoints)
            .collect();
        accounts
            .into_iter()
            .filter_map(|address| self.residual(address, attributed.as_ref()).transpose())
            .collect()
    }

    /// The ether `address` moved that `attributed` and, for the sender, the
    /// fee it paid, leave unexplained.
    ///
    /// Traced balances are total — the gas the sender paid is already in them
    /// — so the fee joins the credit side here and cancels out. The
    /// transaction's [`Fee`] stays the one record of gas, and no residual
    /// repeats it.
    fn residual(
        &self,
        address: &Address,
        attributed: Option<&Movement>,
    ) -> Result<Option<Movement>, Error> {
        let balance = self.trace.balance(address);
        let leg = |endpoint: fn(&Movement) -> Option<&Address>| {
            attributed
                .filter(|value| endpoint(value) == Some(address))
                .map(|value| value.amount.clone())
        };
        let credit = Self::total(&[
            balance.map(|balance| balance.after.clone()),
            leg(|value| value.from.as_ref()),
            (self.fee.payer.as_ref() == Some(address)).then(|| self.fee.amount.clone()),
        ])?;
        let debit = Self::total(&[
            balance.map(|balance| balance.before.clone()),
            leg(|value| value.to.as_ref()),
        ])?;
        let credited = credit > debit;
        let (larger, smaller) = if credited {
            (&credit, &debit)
        } else {
            (&debit, &credit)
        };
        let Some(amount) = larger
            .checked_sub(smaller)
            .filter(|amount| !amount.is_zero())
        else {
            return Ok(None);
        };
        Ok(Some(Movement::residual(
            format!("{}:residual:{address}", self.id),
            address,
            credited,
            amount,
        )))
    }

    fn total(amounts: &[Option<Wei>]) -> Result<Wei, Error> {
        amounts
            .iter()
            .flatten()
            .try_fold(Wei::ZERO, |total, amount| total.checked_add(amount))
            .ok_or_else(|| {
                Error::invalid_block("Ethereum traced balance exceeds 256-bit atomic units")
            })
    }
}

impl Movement {
    /// The accounts this movement names, in `from`, `to` order.
    pub fn endpoints(&self) -> impl Iterator<Item = &Address> {
        self.from.iter().chain(&self.to)
    }

    /// `amount` wei of `address` that its transaction's top-level `value` and
    /// fee do not account for: a credit when `credited`, a debit otherwise.
    fn residual(id: String, address: &Address, credited: bool, amount: Wei) -> Self {
        Self {
            id,
            kind: if credited {
                MovementKind::Mint
            } else {
                MovementKind::Burn
            },
            asset: AssetKind::Native,
            amount,
            from: (!credited).then(|| address.clone()),
            to: credited.then(|| address.clone()),
        }
    }
}

impl Identified for Transaction {
    type Id = TransactionId;

    fn id(&self) -> &TransactionId {
        &self.id
    }
}

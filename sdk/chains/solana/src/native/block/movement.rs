//! Movements: what a transaction's decoded transfers and its runtime-reported
//! balances say moved, and to whom.

use std::collections::BTreeSet;

use base::MovementKind;

use super::{AccountKey, Instruction, TokenBalance, Transaction};
use crate::native::{Address, AssetKind, Error, Lamport, spl::TokenTransfer};

/// One value change inside a [`Transaction`], in native types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Movement {
    /// Stable within the block: `<signature>:ix:<n>` or
    /// `<signature>:ix:<n>:inner:<m>` for a decoded System or SPL Token
    /// transfer, `<signature>:residual:<account index>` for a lamport
    /// residual, and `<signature>:token:<account index>` for the residual of
    /// a token position.
    pub id: String,
    /// [`MovementKind::Transfer`] for a decoded transfer, which names both
    /// endpoints; a residual names one account only, so it credits it as a
    /// [`MovementKind::Mint`] and debits it as a [`MovementKind::Burn`].
    pub kind: MovementKind,
    /// Native SOL, or the mint whose units moved.
    pub asset: AssetKind,
    /// Units of [`Movement::asset`] moved; never zero.
    pub amount: Lamport,
    /// Debited account — the token account's owner for an SPL movement;
    /// `None` for a residual credit.
    pub from: Option<Address>,
    /// Credited account — the token account's owner for an SPL movement;
    /// `None` for a residual debit.
    pub to: Option<Address>,
}

impl Transaction {
    /// Every decoded transfer, outer and inner, in execution order — System
    /// transfers first, then SPL Token transfers — followed by the lamport
    /// residuals and then the token residuals, each in account-key order;
    /// empty when the transaction failed.
    #[must_use]
    pub fn movements(&self) -> &[Movement] {
        &self.movements
    }

    /// Checks that each `selected` account the transaction may have changed
    /// (writable, the payer, or a movement endpoint) changed by exactly its
    /// movements minus the fee it paid, and that each `selected` owner's
    /// position in each mint changed by exactly its token movements.
    ///
    /// The residuals the block parser emits make that identity hold by
    /// construction, so this asserts the arithmetic behind them rather than
    /// filtering which value effects the chain may have. A node that reports
    /// no inner instructions therefore costs attribution, not indexing: the
    /// effects its omitted instructions would have named become residuals.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] when an account's movements
    /// do not sum to its balance change, which the parse path cannot produce.
    pub fn reconcile(&self, selected: &BTreeSet<Address>) -> Result<(), Error> {
        let affected = self.accounts.iter().filter(|key| {
            selected.contains(&key.address)
                && (key.writable || key.address == *self.payer() || self.touches(&key.address))
        });
        for key in affected {
            if self.unexplained(key) != 0 {
                return Err(Error::invalid_block(
                    "selected Solana account balance does not reconcile with its movements",
                ));
            }
        }
        let positions = self
            .tokens
            .iter()
            .filter(|position| selected.contains(&position.owner));
        for position in positions {
            if self.token_unexplained(&position.owner, &position.mint) != 0 {
                return Err(Error::invalid_block(
                    "selected Solana token position does not reconcile with its movements",
                ));
            }
        }
        Ok(())
    }

    /// Whether any movement debits or credits `address`.
    #[must_use]
    pub fn touches(&self, address: &Address) -> bool {
        self.movements.iter().any(|movement| {
            movement.from.as_ref() == Some(address) || movement.to.as_ref() == Some(address)
        })
    }

    /// Lamports `key` moved that its movements, and for the payer the fee it
    /// paid, do not account for: the runtime's own pre/post accounting is the
    /// authority and the decoded instructions attribute part of it.
    fn unexplained(&self, key: &AccountKey) -> i128 {
        let endpoint = |address: Option<&Address>| i128::from(address == Some(&key.address));
        let attributed: i128 = self
            .movements
            .iter()
            .filter(|movement| movement.asset == AssetKind::Native)
            .map(|movement| {
                let net = endpoint(movement.to.as_ref()) - endpoint(movement.from.as_ref());
                net * i128::from(movement.amount.units())
            })
            .sum();
        let fee = i128::from(key.address == *self.payer()) * i128::from(self.fee.amount.units());
        let before = i128::from(key.balance.before.units());
        i128::from(key.balance.after.units()) - before - attributed + fee
    }

    /// Units of `owner`'s position in `mint` that its token movements do not
    /// account for. The transaction's own token balances are the authority
    /// and the decoded transfers attribute part of them.
    fn token_unexplained(&self, owner: &Address, mint: &Address) -> i128 {
        let asset = AssetKind::SplToken(mint.clone());
        let endpoint = |address: Option<&Address>| i128::from(address == Some(owner));
        let attributed: i128 = self
            .movements
            .iter()
            .filter(|movement| movement.asset == asset)
            .map(|movement| {
                let net = endpoint(movement.to.as_ref()) - endpoint(movement.from.as_ref());
                net * i128::from(movement.amount.units())
            })
            .sum();
        let moved: i128 = self
            .tokens
            .iter()
            .filter(|position| position.held_by(owner, mint))
            .map(TokenBalance::delta)
            .sum();
        moved - attributed
    }

    /// Decodes every transfer of a successful transaction, then attributes the
    /// lamports and token units those transfers leave unexplained.
    ///
    /// Only System `Transfer` and `TransferWithSeed` name both endpoints of a
    /// lamport move, and only SPL Token `Transfer` and `TransferChecked` name
    /// both endpoints of a token move. Every other effect — a CPI, an account
    /// close returning its rent, a mint or burn, a token account created or
    /// closed, or any instruction a node omitted from `innerInstructions` —
    /// reaches an account without one. It is an exact change the runtime's own
    /// pre/post accounting reports, so it is recorded as a one-sided
    /// [`Movement`] rather than rejected. That costs attribution, not the
    /// amount: a residual is a [`MovementKind::Mint`] or [`MovementKind::Burn`]
    /// with a `:residual:` or `:token:` id, never a [`MovementKind::Transfer`]
    /// naming both sides.
    pub(super) fn decode_movements(&mut self) -> Result<(), Error> {
        if !self.succeeded {
            return Ok(());
        }
        let (tokens, transfers): (Vec<_>, Vec<_>) = self.token_transfers().into_iter().unzip();
        self.movements = self.native_transfers()?;
        self.movements.extend(tokens);
        let residuals = self.residuals()?;
        self.movements.extend(residuals);
        let residuals = self.token_residuals(&transfers)?;
        self.movements.extend(residuals);
        Ok(())
    }

    /// Every positive System transfer, outer and inner, in execution order.
    fn native_transfers(&self) -> Result<Vec<Movement>, Error> {
        let mut movements = Vec::new();
        for (outer, instruction) in self.instructions.iter().enumerate() {
            for (id, call) in self.calls(outer, instruction) {
                let Some(transfer) = call.system_transfer(&self.accounts)? else {
                    continue;
                };
                movements.push(Movement {
                    id,
                    kind: MovementKind::Transfer,
                    asset: AssetKind::Native,
                    amount: transfer.amount,
                    from: Some(transfer.from),
                    to: Some(transfer.to),
                });
            }
        }
        Ok(movements)
    }

    /// Every SPL Token transfer between two token accounts this transaction
    /// reports, in execution order, with the movement each records.
    fn token_transfers(&self) -> Vec<(Movement, TokenTransfer)> {
        let mut decoded = Vec::new();
        for (outer, instruction) in self.instructions.iter().enumerate() {
            for (id, call) in self.calls(outer, instruction) {
                let Some(transfer) = call.spl_transfer(&self.accounts) else {
                    continue;
                };
                let Some(movement) = transfer.movement(id, &self.tokens) else {
                    continue;
                };
                decoded.push((movement, transfer));
            }
        }
        decoded
    }

    /// The outer instruction `outer` and the instructions it invoked, each
    /// with the movement id a transfer it names would carry.
    fn calls<'a>(
        &self,
        outer: usize,
        instruction: &'a Instruction,
    ) -> impl Iterator<Item = (String, &'a Instruction)> {
        let prefix = format!("{}:ix:{outer}", self.id);
        std::iter::once((prefix.clone(), instruction)).chain(
            instruction
                .inner
                .iter()
                .enumerate()
                .map(move |(ordinal, inner)| (format!("{prefix}:inner:{ordinal}"), inner)),
        )
    }

    /// One movement per account key whose lamports moved by more than its
    /// decoded transfers, and for the payer the fee, explain, in account-key
    /// order.
    fn residuals(&self) -> Result<Vec<Movement>, Error> {
        self.accounts
            .iter()
            .enumerate()
            .filter_map(|(index, key)| match self.unexplained(key) {
                0 => None,
                residual => Some(Movement::residual(
                    format!("{}:residual:{index}", self.id),
                    AssetKind::Native,
                    &key.address,
                    residual,
                )),
            })
            .collect()
    }

    /// One movement per token position whose units moved by more than its
    /// decoded transfers explain, in account-key order.
    fn token_residuals(&self, transfers: &[TokenTransfer]) -> Result<Vec<Movement>, Error> {
        self.tokens
            .iter()
            .filter_map(|position| match position.unexplained(transfers) {
                0 => None,
                residual => Some(Movement::residual(
                    format!("{}:token:{}", self.id, position.account),
                    AssetKind::SplToken(position.mint.clone()),
                    &position.owner,
                    residual,
                )),
            })
            .collect()
    }
}

impl Movement {
    /// The `residual` units of `asset` at `address` that its transaction's
    /// decoded transfers, and for the fee payer the fee, do not account for:
    /// a credit when positive, a debit when negative.
    fn residual(
        id: String,
        asset: AssetKind,
        address: &Address,
        residual: i128,
    ) -> Result<Self, Error> {
        let amount = u64::try_from(residual.unsigned_abs()).map_err(|_| {
            Error::invalid_block("Solana balance change exceeds the 64-bit unit range")
        })?;
        let credited = residual > 0;
        Ok(Self {
            id,
            kind: if credited {
                MovementKind::Mint
            } else {
                MovementKind::Burn
            },
            asset,
            amount: Lamport::from_atomic(amount),
            from: (!credited).then(|| address.clone()),
            to: credited.then(|| address.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use solana_address::Address as NativeAddress;
    use solana_signature::Signature;
    use solana_system_interface::{instruction::SystemInstruction, program::ID as SYSTEM_ID};

    use super::*;
    use crate::native::block::wire::TransactionWire;

    fn account(byte: u8) -> Address {
        NativeAddress::from([byte; 32])
            .to_string()
            .parse()
            .expect("canonical address")
    }

    /// A 10-lamport System transfer from the fee payer, paying 7 lamports of
    /// fee, whose reported `post` balances the caller chooses.
    fn parse(post: [u64; 3]) -> Transaction {
        let transfer = SystemInstruction::Transfer { lamports: 10 };
        let data =
            bs58::encode(bincode::serialize(&transfer).expect("transfer encodes")).into_string();
        let wire = json!({
            "transaction": {
                "signatures": [Signature::from([9; 64]).to_string()],
                "message": {
                    "header": {
                        "numRequiredSignatures": 1,
                        "numReadonlySignedAccounts": 0,
                        "numReadonlyUnsignedAccounts": 1,
                    },
                    "accountKeys": [
                        account(1).to_string(),
                        account(2).to_string(),
                        SYSTEM_ID.to_string(),
                    ],
                    "recentBlockhash": "11111111111111111111111111111111",
                    "instructions": [{"programIdIndex": 2, "accounts": [0, 1], "data": data}],
                },
            },
            "meta": {
                "err": null,
                "fee": 7,
                "preBalances": [100, 0, 0],
                "postBalances": post,
                "innerInstructions": [],
            },
            "version": "legacy",
        });
        let mut transaction = Transaction::parse(
            serde_json::from_value::<TransactionWire>(wire).expect("fixture shape"),
        )
        .expect("fixture parses");
        transaction.decode_movements().expect("fixture decodes");
        transaction
    }

    #[test]
    fn decoded_transfers_alone_leave_no_residual() {
        let transaction = parse([83, 10, 0]);
        assert_eq!(transaction.movements().len(), 1);
        assert_eq!(transaction.movements()[0].kind, MovementKind::Transfer);
        transaction
            .reconcile(&BTreeSet::from([account(1), account(2)]))
            .expect("the transfer and the fee explain both accounts");
    }

    #[test]
    fn residuals_settle_every_account_and_keep_reconciliation_an_assertion() {
        // One lamport more than the decoded transfer reached the destination,
        // and it came from the payer over and above the fee it paid.
        let mut transaction = parse([82, 11, 0]);
        let selected = BTreeSet::from([account(1), account(2)]);
        let ids = transaction
            .movements()
            .iter()
            .map(|movement| movement.id.clone())
            .collect::<Vec<_>>();
        let signature = Signature::from([9; 64]).to_string();
        assert_eq!(
            ids,
            [
                format!("{signature}:ix:0"),
                format!("{signature}:residual:0"),
                format!("{signature}:residual:1"),
            ]
        );
        assert_eq!(transaction.movements()[1].kind, MovementKind::Burn);
        assert_eq!(transaction.movements()[1].from.as_ref(), Some(&account(1)));
        assert_eq!(transaction.movements()[1].to, None);
        assert_eq!(transaction.movements()[1].amount.units(), 1);
        assert_eq!(transaction.movements()[2].kind, MovementKind::Mint);
        assert_eq!(transaction.movements()[2].to.as_ref(), Some(&account(2)));
        assert_eq!(transaction.movements()[2].from, None);
        assert_eq!(transaction.movements()[2].amount.units(), 1);
        transaction
            .reconcile(&selected)
            .expect("residuals settle every account by construction");

        // Only corrupting a parsed balance afterwards can reach the assertion.
        transaction.accounts[1].balance.after = Lamport::from_atomic(12);
        assert_eq!(
            transaction
                .reconcile(&selected)
                .expect_err("unsound arithmetic")
                .message,
            "selected Solana account balance does not reconcile with its movements"
        );
    }
}

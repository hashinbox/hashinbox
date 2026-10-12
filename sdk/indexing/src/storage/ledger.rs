//! One balance change of one holding, as both shipped stores keep it
//! (`ledger` rows in SQLite, ledger entries in memory): derived from a batch
//! once, ordered once and read back into history once, so the two stores
//! cannot disagree on a row, its order or its running total.
//!
//! **Chain order** within a holding is `(position, movement_order, id)`:
//! `movement_order` is 2 x the movement's index in its transaction plus 1 for
//! the debit side (a self-transfer's credit first, a zero amount keeping its
//! side), [`FEE_ORDER`] for a fee (last), 0 for a withdrawal; `id` breaks ties.

use std::collections::BTreeMap;

use base::MovementKind;
use num_bigint::BigInt;

use crate::{
    Address, AssetId, BlockCredit, BlockPosition, BlockRef, BlockWithOutputs, Decimal,
    FeeSettlement, Holder, IndexError, IndexErrorKind, MovementId, NetworkFee, Transaction,
    ValueMovement,
};

/// What one transaction did to one address, as history shows it: its
/// movements, and the fee it was charged.
pub(crate) type Effects = (Vec<ValueMovement>, Option<NetworkFee>);

/// The `movement_id` of a fee charged beside the movements.
///
/// `Transaction::validate_movements` refuses a movement whose identity is
/// empty, so a fee can never collide with a movement of its own transaction.
const CHARGED: &str = "";

/// The `movement_id` of a fee the movements already contain (Bitcoin's
/// inputs minus outputs): recorded so history shows it, never added to a
/// total again. A movement id is the chain's own, and no chain names a
/// movement this, because a fee entry has kind [`Kind::Fee`].
const EMBEDDED: &str = "embedded";

/// The `movement_order` of a fee: last in its block, and odd, so a debit.
const FEE_ORDER: i64 = i64::MAX;

/// The canonical id of `parts`: each part with `\\` and `:` escaped by a
/// backslash, joined by `:`. Two different part lists never share an id,
/// whatever text the parts hold.
pub(crate) fn canonical_id(parts: &[&str]) -> String {
    let mut id = String::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            id.push(':');
        }
        id.extend(part.chars().flat_map(|character| {
            let escape = matches!(character, '\\' | ':').then_some('\\');
            escape.into_iter().chain([character])
        }));
    }
    id
}

/// Which way value crossed the address: the sign of the change, and the low
/// bit of `movement_order`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
    /// Value reached the address; sorts first within a movement.
    Credit,
    /// Value left the address.
    Debit,
}

impl Direction {
    /// The side one `movement_order` names.
    const fn of_order(order: i64) -> Self {
        if order % 2 == 0 {
            Self::Credit
        } else {
            Self::Debit
        }
    }

    /// The `movement_order` of this side of the movement at `index`.
    fn order(self, index: usize) -> Result<i64, IndexError> {
        i64::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(2))
            .and_then(|order| order.checked_add(i64::from(self == Self::Debit)))
            .ok_or_else(|| store("a movement index is above the storable range"))
    }

    /// The id part naming this side.
    const fn tag(self) -> &'static str {
        match self {
            Self::Credit => "0",
            Self::Debit => "1",
        }
    }

    /// `amount`, signed for this side.
    fn diff(self, amount: &Decimal) -> Result<BigInt, IndexError> {
        let units = BigInt::from(
            amount
                .to_atomic(0)
                .map_err(|_| store("a movement amount is not an atomic integer"))?,
        );
        Ok(match self {
            Self::Credit => units,
            Self::Debit => -units,
        })
    }
}

/// What an entry records: SQLite's `kind`.
///
/// The stored spelling of the five movement kinds is [`MovementKind`]'s order;
/// the fee and the withdrawal are balance changes without being movements, so
/// they are this store's own two tags after them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `0..=4`: a movement of its transaction.
    Movement(MovementKind),
    /// `5`: the network fee.
    Fee,
    /// `6`: value a block moved with no transaction to name it.
    Withdrawal,
}

impl Kind {
    /// The kind one stored tag names.
    #[cfg(feature = "sqlite")]
    pub(crate) fn from_tag(tag: i64) -> Result<Self, IndexError> {
        Ok(match tag {
            0 => Self::Movement(MovementKind::Transfer),
            1 => Self::Movement(MovementKind::Input),
            2 => Self::Movement(MovementKind::Output),
            3 => Self::Movement(MovementKind::Mint),
            4 => Self::Movement(MovementKind::Burn),
            5 => Self::Fee,
            6 => Self::Withdrawal,
            _ => return Err(store("a stored movement has an unknown kind")),
        })
    }

    /// Whether an entry of this kind under `movement_id` counts towards its
    /// holding's total: all but a fee the movements already contain.
    pub(crate) fn counts(self, movement_id: &str) -> bool {
        !(self == Self::Fee && movement_id == EMBEDDED)
    }

    /// The stored tag.
    #[cfg(feature = "sqlite")]
    pub(crate) const fn tag(self) -> i64 {
        match self {
            Self::Movement(MovementKind::Transfer) => 0,
            Self::Movement(MovementKind::Input) => 1,
            Self::Movement(MovementKind::Output) => 2,
            Self::Movement(MovementKind::Mint) => 3,
            Self::Movement(MovementKind::Burn) => 4,
            Self::Fee => 5,
            Self::Withdrawal => 6,
        }
    }
}

/// One change to one holding. Equality is the whole entry: one id —
/// `(block, transaction, movement, address, asset, side)` — cannot honestly
/// have two changes, kinds or counterparties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LedgerEntry {
    /// Unique per entry; the last part of the chain order.
    pub(crate) id: String,
    /// The holding's address.
    pub(crate) address: Address,
    /// The holding's asset.
    pub(crate) asset: AssetId,
    /// The including block's native position.
    pub(crate) position: BlockPosition,
    /// The transaction's native id; `None` for a withdrawal.
    pub(crate) transaction: Option<String>,
    /// The chain's movement id, or the fee or credit marker.
    pub(crate) movement_id: String,
    /// The in-block order (module documentation).
    pub(crate) movement_order: i64,
    /// What it records.
    pub(crate) kind: Kind,
    /// The signed change, as history shows it.
    pub(crate) diff: BigInt,
    /// The other endpoint of a movement, when it has one.
    pub(crate) counterparty: Option<String>,
}

impl LedgerEntry {
    /// Every entry `block` makes, to every address: each transaction's
    /// effects on each address that took part in it, then the credits.
    ///
    /// **The fee is always an entry, and counted only where it is charged**
    /// ([`Self::counted`]). One per transaction per payer, never one per
    /// participant.
    pub(crate) fn of_block(
        block: &BlockWithOutputs,
        settlement: FeeSettlement,
    ) -> Result<Vec<Self>, IndexError> {
        let reference = block.block.reference();
        let mut entries = Vec::new();
        for entry in block.block.transactions() {
            for address in entry.addresses() {
                entries.extend(Self::effects(reference, &address, entry, settlement)?);
            }
        }
        for credit in &block.credits {
            entries.push(Self::withdrawal(reference, credit)?);
        }
        Ok(entries)
    }

    /// The entry of one effect, its id derived from every part of its key.
    fn at(
        block: &BlockRef,
        (address, asset): (&Address, &AssetId),
        transaction: Option<&str>,
        (movement_id, movement_order): (&str, i64),
        (kind, diff): (Kind, BigInt),
    ) -> Self {
        let network = &address.network;
        // Lowercase hex, as `hex::encode` spells SQLite's block ids; written
        // out so the memory store needs no `hex` dependency.
        let hash: String = block
            .hash
            .0
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let id = canonical_id(&[
            network.chain.as_str(),
            &network.name,
            &hash,
            transaction.unwrap_or_default(),
            movement_id,
            &address.value,
            &asset.asset,
            Direction::of_order(movement_order).tag(),
        ]);
        Self {
            id,
            address: address.clone(),
            asset: asset.clone(),
            position: block.position,
            transaction: transaction.map(str::to_owned),
            movement_id: movement_id.to_owned(),
            movement_order,
            kind,
            diff,
            counterparty: None,
        }
    }

    /// Every effect `entry` has on `address`.
    fn effects(
        block: &BlockRef,
        address: &Address,
        entry: &Transaction,
        settlement: FeeSettlement,
    ) -> Result<Vec<Self>, IndexError> {
        let transaction = Some(entry.id.id.as_str());
        let mut effects = Vec::new();
        for (index, movement) in entry.movements.iter().enumerate() {
            let sides = [
                (Direction::Credit, movement.to(), movement.from()),
                (Direction::Debit, movement.from(), movement.to()),
            ];
            for (direction, side, other) in sides {
                if side != Some(address) {
                    continue;
                }
                let order = (movement.id.0.as_str(), direction.order(index)?);
                let change = (
                    Kind::Movement(movement.kind()),
                    direction.diff(&movement.amount)?,
                );
                let holding = (address, &movement.asset);
                let effect = Self::at(block, holding, transaction, order, change);
                effects.push(Self {
                    counterparty: other.map(|other| other.value.clone()),
                    ..effect
                });
            }
        }
        let paid = entry
            .fee
            .as_ref()
            .filter(|fee| fee.payer.as_ref() == Some(address));
        if let Some(fee) = paid {
            let marker = match settlement {
                FeeSettlement::Charged => CHARGED,
                FeeSettlement::Embedded => EMBEDDED,
            };
            effects.push(Self::at(
                block,
                (address, &fee.asset),
                transaction,
                (marker, FEE_ORDER),
                (Kind::Fee, Direction::Debit.diff(&fee.amount)?),
            ));
        }
        Ok(effects)
    }

    /// One transactionless credit.
    fn withdrawal(block: &BlockRef, credit: &BlockCredit) -> Result<Self, IndexError> {
        Ok(Self::at(
            block,
            (&credit.to, &credit.asset),
            None,
            (&credit.id.0, 0),
            (Kind::Withdrawal, Direction::Credit.diff(&credit.amount)?),
        ))
    }

    /// What this entry adds to its holding's running total: its change, or
    /// nothing for a fee the movements already contain.
    #[cfg(feature = "memory")]
    pub(crate) fn counted(&self) -> BigInt {
        if self.kind.counts(&self.movement_id) {
            return self.diff.clone();
        }
        BigInt::ZERO
    }

    /// Where this entry sits in its holding's chain order.
    #[cfg(feature = "memory")]
    pub(crate) fn place(&self) -> (BlockPosition, i64, &str) {
        (self.position, self.movement_order, &self.id)
    }

    /// The unsigned amount, as history shows it.
    fn amount(&self) -> Decimal {
        Decimal::from_atomic(self.diff.magnitude().clone(), 0)
    }

    /// What each transaction did to `of`, by native transaction id, from
    /// `entries` of its address: its movements in its asset (a self-transfer's
    /// two entries folded back into one movement) and the fee it paid in any
    /// asset. Withdrawals are not here.
    pub(crate) fn history<'a>(
        entries: impl IntoIterator<Item = &'a Self>,
        of: &dyn Holder,
    ) -> Result<BTreeMap<String, Effects>, IndexError> {
        let address = of.address();
        let mut entries = entries
            .into_iter()
            .filter(|entry| &entry.address == address && entry.transaction.is_some())
            .filter(|entry| match entry.kind {
                Kind::Movement(_) => &entry.asset == of.asset(),
                Kind::Fee => true,
                Kind::Withdrawal => false,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            (&left.transaction, left.movement_order, &left.movement_id).cmp(&(
                &right.transaction,
                right.movement_order,
                &right.movement_id,
            ))
        });
        let mut grouped: BTreeMap<String, Effects> = BTreeMap::new();
        let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
        for entry in entries {
            let transaction = entry.transaction.clone().unwrap_or_default();
            let held = grouped.entry(transaction.clone()).or_default();
            let Kind::Movement(kind) = entry.kind else {
                held.1 = Some(NetworkFee {
                    asset: entry.asset.clone(),
                    amount: entry.amount(),
                    payer: Some(address.clone()),
                });
                continue;
            };
            let mine = Some(address.clone());
            let other = entry
                .counterparty
                .clone()
                .map(|value| Address::new(address.network.clone(), value));
            let key = (transaction, entry.movement_id.clone());
            let (from, to, at) = match (seen.get(&key), Direction::of_order(entry.movement_order)) {
                // The other side of the same movement: a self-transfer.
                (Some(at), _) => (mine.clone(), mine, Some(*at)),
                (None, Direction::Credit) => (other, mine, None),
                (None, Direction::Debit) => (mine, other, None),
            };
            let value = ValueMovement::stored(
                MovementId(entry.movement_id.clone()),
                of.asset().clone(),
                entry.amount(),
                kind,
                from,
                to,
            )?;
            match at {
                Some(at) => held.0[at] = value,
                None => {
                    seen.insert(key, held.0.len());
                    held.0.push(value);
                }
            }
        }
        Ok(grouped)
    }
}

/// An entry that cannot be stored or read: terminal.
fn store(message: &str) -> IndexError {
    IndexError::new(IndexErrorKind::Store, message, false)
}

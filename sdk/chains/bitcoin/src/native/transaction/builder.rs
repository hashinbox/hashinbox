use std::{cmp::Ordering, collections::BTreeSet};

use base::{BoxFuture, Signer};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    transaction::Version,
};

use super::{FeeRate, Funds, SignedTransaction, Transfer, UnsignedTransaction};
use crate::native::{Address, Error, ErrorKind, Network, Satoshi, block::Utxo};

/// Weight of the segwit marker and flag bytes.
const SEGWIT_MARKER_FLAG_WEIGHT: u64 = 2;

/// Everything besides the transfers the builder needs: the network, the
/// outputs it may spend, where change goes, and the fee rate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BuildContext {
    /// The network the transaction is valid on.
    pub network: Network,
    /// Spendable outputs; the builder selects from them.
    pub available: Vec<Utxo>,
    /// Receives change above the dust limit.
    pub change: Address,
    /// Applied to the predicted virtual size.
    pub fee_rate: FeeRate,
}

/// Selects inputs for [`Transfer`]s, adds change, and signs. Pure: no RPC.
///
/// Normal selection spends the largest outputs first until the transfers and
/// fee are covered. [`Builder::draining`] spends every output instead and
/// pays everything but the fee to its one transfer.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Builder {
    transfers: Vec<Transfer>,
    context: BuildContext,
    drain: bool,
}

impl Builder {
    /// Pays `transfers` from `context.available`.
    #[must_use]
    pub const fn new(transfers: Vec<Transfer>, context: BuildContext) -> Self {
        Self {
            transfers,
            context,
            drain: false,
        }
    }

    /// Spends every available output to the single transfer.
    #[must_use]
    pub fn draining(mut self) -> Self {
        self.drain = true;
        self
    }

    /// The unsigned transaction.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InsufficientFunds`] when the outputs cannot cover the
    /// transfers and fee; [`ErrorKind::InvalidAddress`] and
    /// [`ErrorKind::InvalidTransaction`] for invalid inputs or outputs.
    pub fn build(&self) -> Result<UnsignedTransaction, Error> {
        self.validate()?;
        let network = self.context.network;
        let scripts = self
            .transfers
            .iter()
            .map(|transfer| transfer.script(network, self.drain))
            .collect::<Result<Vec<_>, Error>>()?;
        let change = self.context.change.script_pubkey(network)?;
        if self.drain {
            return self.drain(&scripts);
        }
        self.select(scripts, change)
    }

    /// Builds and signs every input with `signer`.
    pub fn sign<'a>(
        &'a self,
        signer: &'a dyn Signer,
    ) -> BoxFuture<'a, Result<SignedTransaction, Error>> {
        Box::pin(async move { self.build()?.sign(self.context.network, signer).await })
    }

    /// One transaction spending every group's outputs to its transfers plus
    /// change: the grouped counterpart of [`Self::build`], for a send whose
    /// sources are several. The fee is charged to sources in order; a change
    /// output dropped as dust stays dropped even if the smaller transaction
    /// then costs less.
    ///
    /// Construction belongs to this builder; [`Funds`] states what each
    /// source must afford.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InsufficientFunds`] when a source cannot
    /// cover its transfers or the sources cannot cover the fee;
    /// [`crate::native::ErrorKind::InvalidTransaction`] for an empty group,
    /// a zero fee rate, or an output spent twice.
    pub fn grouped(
        network: Network,
        mut groups: Vec<Funds>,
        fee_rate: FeeRate,
    ) -> Result<UnsignedTransaction, Error> {
        if groups.is_empty() || fee_rate.sat_per_kvb() == 0 {
            return Err(Error::invalid_transaction(
                "Bitcoin grouped transfer needs sources and a positive fee rate",
            ));
        }
        let mut available = Vec::new();
        let mut transfers = Vec::new();
        let mut surplus = Vec::with_capacity(groups.len());
        let mut change = Vec::with_capacity(groups.len());
        let required = groups.iter().map(|group| group.sweep.is_some()).collect();
        for group in &mut groups {
            surplus.push(group.surplus()?);
            change.push(
                group
                    .sweep
                    .as_ref()
                    .unwrap_or(&group.change)
                    .script_pubkey(network)?,
            );
            available.append(&mut group.available);
            transfers.append(&mut group.transfers);
        }
        Utxo::require_unique(&available)?;
        let scripts = transfers
            .iter()
            .map(|transfer| transfer.script(network, false))
            .collect::<Result<Vec<_>, _>>()?;
        let shares = Shares {
            surplus,
            change,
            fee_rate,
            required,
        };
        let remaining = shares.change(&available, &scripts)?;
        for (group, value) in groups.iter().zip(remaining) {
            transfers.extend(value.map(|value| {
                Transfer::new(group.sweep.as_ref().unwrap_or(&group.change).clone(), value)
            }));
        }
        Ok(UnsignedTransaction::from_selected(available, transfers))
    }

    fn validate(&self) -> Result<(), Error> {
        let available = &self.context.available;
        if available.is_empty() {
            return Err(Error::new(
                ErrorKind::InsufficientFunds,
                "Bitcoin transfer has no available UTXOs",
            ));
        }
        if self.transfers.is_empty() {
            return Err(Error::invalid_transaction(
                "Bitcoin transfer has no recipients",
            ));
        }
        if self.context.fee_rate.sat_per_kvb() == 0 {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "Bitcoin fee rate must be greater than zero",
            ));
        }
        Utxo::require_unique(available)?;
        if available
            .iter()
            .any(|utxo| utxo.satisfaction_weight().is_none())
        {
            return Err(Error::invalid_transaction(
                "Bitcoin wallet supports P2WPKH and P2TR inputs only",
            ));
        }
        Ok(())
    }

    fn drain(&self, scripts: &[ScriptBuf]) -> Result<UnsignedTransaction, Error> {
        if self.transfers.len() != 1 {
            return Err(Error::invalid_transaction(
                "Bitcoin drain transfer requires exactly one recipient",
            ));
        }
        let mut available = self.context.available.clone();
        available.sort_by(Utxo::display_order);
        let total = Utxo::total(&available)?;
        let fee = self.context.fee_rate.predict(&available, scripts)?;
        let value = total.checked_sub(fee.units()).ok_or_else(|| {
            Error::insufficient("Bitcoin UTXOs cannot cover the drain transaction fee".to_owned())
        })?;
        let minimum = scripts[0].minimal_non_dust().to_sat();
        if value < minimum {
            return Err(Error::insufficient(format!(
                "Bitcoin drain output is dust: minimum is {minimum} satoshis"
            )));
        }
        let transfer = Transfer::new(self.transfers[0].to.clone(), Satoshi::from_atomic(value));
        Ok(UnsignedTransaction::from_selected(
            available,
            vec![transfer],
        ))
    }

    fn select(
        &self,
        scripts: Vec<ScriptBuf>,
        change: ScriptBuf,
    ) -> Result<UnsignedTransaction, Error> {
        let mut available = self.context.available.clone();
        available.sort_by(|left, right| {
            right
                .output
                .value
                .cmp(&left.output.value)
                .then_with(|| left.outpoint.cmp(&right.outpoint))
        });
        let requested = Transfer::total(&self.transfers)?;
        let mut with_change = scripts.clone();
        with_change.push(change.clone());
        for count in 1..=available.len() {
            let selected = &available[..count];
            let total = Utxo::total(selected)?;
            let fee = self.context.fee_rate.predict(selected, &scripts)?;
            let required = requested.checked_add(fee.units()).ok_or_else(|| {
                Error::invalid_transaction("Bitcoin amount and fee overflowed u64")
            })?;
            if total >= required {
                let fee = self.context.fee_rate.predict(selected, &with_change)?;
                let change = (total - requested)
                    .checked_sub(fee.units())
                    .filter(|value| *value >= change.minimal_non_dust().to_sat());
                let mut outputs = self.transfers.clone();
                outputs.extend(change.map(|value| {
                    Transfer::new(self.context.change.clone(), Satoshi::from_atomic(value))
                }));
                return Ok(UnsignedTransaction::from_selected(
                    selected.to_vec(),
                    outputs,
                ));
            }
        }
        Err(Error::insufficient(format!(
            "insufficient Bitcoin funds for {requested} satoshis plus network fee"
        )))
    }
}

impl Transfer {
    /// The sum of every transfer's amount.
    pub(super) fn total(transfers: &[Self]) -> Result<u64, Error> {
        transfers.iter().try_fold(0_u64, |total, transfer| {
            total.checked_add(transfer.amount.units()).ok_or_else(|| {
                Error::invalid_transaction("Bitcoin recipient amount overflowed u64")
            })
        })
    }
}

impl Utxo {
    /// Displayed txid, then output index: the order drains and groups spend in.
    pub(super) fn display_order(&self, other: &Self) -> Ordering {
        let (left, right) = (&self.outpoint, &other.outpoint);
        left.transaction_id
            .to_string()
            .cmp(&right.transaction_id.to_string())
            .then_with(|| left.output_index.cmp(&right.output_index))
    }

    /// The sum of every output's value.
    pub(super) fn total(utxos: &[Self]) -> Result<u64, Error> {
        utxos
            .iter()
            .try_fold(0_u64, |total, utxo| {
                total.checked_add(utxo.output.value.units())
            })
            .ok_or_else(|| {
                Error::invalid_transaction("Bitcoin selected input amount overflowed u64")
            })
    }

    /// Rejects spending one outpoint twice.
    pub(super) fn require_unique(utxos: &[Self]) -> Result<(), Error> {
        let mut seen = BTreeSet::new();
        if utxos.iter().any(|utxo| !seen.insert(utxo.outpoint)) {
            return Err(Error::invalid_transaction(
                "Bitcoin transfer contains a duplicate UTXO",
            ));
        }
        Ok(())
    }
}

impl FeeRate {
    /// The fee of a transaction spending `inputs` to `outputs`: its predicted
    /// weight with signatures, converted to virtual bytes.
    pub(super) fn predict(self, inputs: &[Utxo], outputs: &[ScriptBuf]) -> Result<Satoshi, Error> {
        let transaction = Transaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: inputs
                .iter()
                .map(|utxo| TxIn {
                    previous_output: OutPoint::new(
                        Txid::from(utxo.outpoint.transaction_id),
                        utxo.outpoint.output_index,
                    ),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
                .collect(),
            output: outputs
                .iter()
                .map(|script_pubkey| TxOut {
                    value: Amount::ZERO,
                    script_pubkey: script_pubkey.clone(),
                })
                .collect(),
        };
        let overflow = || Error::invalid_transaction("Bitcoin transaction weight overflowed u64");
        let witnesses = inputs.iter().try_fold(0_u64, |total, utxo| {
            total
                .checked_add(utxo.satisfaction_weight().unwrap_or_default())
                .ok_or_else(overflow)
        })?;
        let weight = transaction
            .weight()
            .to_wu()
            .checked_add(SEGWIT_MARKER_FLAG_WEIGHT)
            .and_then(|weight| weight.checked_add(witnesses))
            .ok_or_else(overflow)?;
        self.fee(weight.div_ceil(4))
    }
}

/// What each source has left after its transfers, and its change script.
struct Shares {
    surplus: Vec<u64>,
    change: Vec<ScriptBuf>,
    fee_rate: FeeRate,
    required: Vec<bool>,
}

impl Shares {
    /// Each source's change after the fee, `None` where it would be dust.
    fn change(
        &self,
        inputs: &[Utxo],
        scripts: &[ScriptBuf],
    ) -> Result<Vec<Option<Satoshi>>, Error> {
        let mut active = self.above_dust(&self.surplus, None);
        loop {
            let mut outputs = scripts.to_vec();
            let kept = self.change.iter().zip(&active).filter(|(_, keep)| **keep);
            outputs.extend(kept.map(|(script, _)| script.clone()));
            let fee = self.fee_rate.predict(inputs, &outputs)?;
            let remaining = Self::charge(&self.surplus, fee.units())?;
            let next = self.above_dust(&remaining, Some(&active));
            if next == active {
                let dust = self.required.iter().zip(&remaining).zip(&self.change).any(
                    |((required, value), script)| {
                        *required && *value < script.minimal_non_dust().to_sat()
                    },
                );
                if dust {
                    return Err(Error::insufficient(
                        "Bitcoin sweep remainder cannot cover a non-dust output",
                    ));
                }
                let change = remaining.into_iter().zip(active);
                return Ok(change
                    .map(|(value, keep)| keep.then_some(Satoshi::from_atomic(value)))
                    .collect());
            }
            active = next;
        }
    }

    /// Which change outputs stay: above dust and, after the first pass, still active.
    fn above_dust(&self, values: &[u64], active: Option<&[bool]>) -> Vec<bool> {
        values
            .iter()
            .zip(&self.change)
            .enumerate()
            .map(|(index, (value, script))| {
                let was_active = active.is_none_or(|active| active[index]);
                self.required[index] || (was_active && *value >= script.minimal_non_dust().to_sat())
            })
            .collect()
    }

    /// Charges `fee` to the surpluses in source order.
    fn charge(surplus: &[u64], fee: u64) -> Result<Vec<u64>, Error> {
        let mut remaining_fee = fee;
        let remaining = surplus
            .iter()
            .map(|value| {
                let contribution = (*value).min(remaining_fee);
                remaining_fee -= contribution;
                value - contribution
            })
            .collect();
        if remaining_fee != 0 {
            return Err(Error::insufficient(
                "Bitcoin grouped sources cannot cover the network fee",
            ));
        }
        Ok(remaining)
    }
}

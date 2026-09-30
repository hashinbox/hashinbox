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

#[cfg(test)]
pub(super) mod tests {
    use bitcoin::hashes::Hash;
    use futures_executor::block_on;

    use super::*;
    use crate::native::{
        TransactionId,
        block::{Outpoint, Output},
    };

    /// A regtest P2WPKH address and script for the key hash `[byte; 20]`.
    pub(in crate::native::transaction) fn p2wpkh(byte: u8) -> (Address, ScriptBuf) {
        let script = ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([byte; 20]));
        (
            Address::from_script(&script, Network::Regtest).unwrap(),
            script,
        )
    }

    pub(in crate::native::transaction) fn utxo(
        id: [u8; 32],
        index: u32,
        value: u64,
        script: &ScriptBuf,
    ) -> Utxo {
        Utxo {
            outpoint: Outpoint {
                transaction_id: TransactionId(id),
                output_index: index,
            },
            output: Output {
                value: Satoshi::from_atomic(value),
                script_pubkey: script.to_bytes(),
                address: None,
            },
        }
    }

    fn context(available: Vec<Utxo>, change: &Address, rate: u64) -> BuildContext {
        BuildContext {
            network: Network::Regtest,
            available,
            change: change.clone(),
            fee_rate: FeeRate::from_sat_per_kvb(rate),
        }
    }

    fn pay(address: &Address, value: u64) -> Transfer {
        Transfer::new(address.clone(), Satoshi::from_atomic(value))
    }

    #[test]
    fn drain_preserves_displayed_outpoint_order_and_exact_fee() {
        let (address, script) = p2wpkh(7);
        let (mut first, mut last) = ([0; 32], [0; 32]);
        first[0] = 1;
        last[31] = 1;
        let available = vec![
            utxo(last, 0, 100_000, &script),
            utxo(first, 10, 100_000, &script),
            utxo(first, 2, 100_000, &script),
        ];
        let builder = Builder::new(vec![pay(&address, 0)], context(available, &address, 1_000));
        let transaction = builder.draining().build().unwrap();
        let order: Vec<_> = transaction
            .inputs
            .iter()
            .map(|spend| {
                (
                    spend.utxo.outpoint.transaction_id.0,
                    spend.utxo.outpoint.output_index,
                )
            })
            .collect();
        assert_eq!(order, vec![(first, 2), (first, 10), (last, 0)]);
        // Three P2WPKH inputs and one output predict 247 virtual bytes.
        assert_eq!(transaction.outputs, vec![pay(&address, 299_753)]);
    }

    #[test]
    fn drain_keeps_fee_insufficiency_and_exact_dust_threshold() {
        let (address, script) = p2wpkh(7);
        for (value, message) in [
            (
                109,
                Some("Bitcoin UTXOs cannot cover the drain transaction fee"),
            ),
            (
                403,
                Some("Bitcoin drain output is dust: minimum is 294 satoshis"),
            ),
            (404, None),
        ] {
            let available = vec![utxo([1; 32], 2, value, &script)];
            let builder = Builder::new(vec![pay(&address, 0)], context(available, &address, 1_000));
            match (builder.draining().build(), message) {
                (Err(error), Some(message)) => {
                    assert_eq!(
                        (error.kind, error.message.as_str()),
                        (ErrorKind::InsufficientFunds, message)
                    );
                }
                (Ok(transaction), None) => {
                    assert_eq!(transaction.outputs, vec![pay(&address, 294)])
                }
                (result, _) => panic!("unexpected {result:?}"),
            }
        }
    }

    #[test]
    fn normal_selection_keeps_amount_then_raw_outpoint_order() {
        let (address, script) = p2wpkh(7);
        let (mut first, mut last) = ([0; 32], [0; 32]);
        first[0] = 1;
        last[31] = 1;
        let available = vec![
            utxo(first, 0, 100_000, &script),
            utxo([0; 32], 0, 40_000, &script),
            utxo(last, 0, 100_000, &script),
        ];
        let builder = Builder::new(
            vec![pay(&address, 150_000)],
            context(available, &address, 1_000),
        );
        let transaction = builder.build().unwrap();
        let order: Vec<_> = transaction
            .inputs
            .iter()
            .map(|spend| spend.utxo.outpoint.transaction_id.0)
            .collect();
        assert_eq!(order, vec![last, first]);
        // Two P2WPKH inputs and two outputs predict 209 virtual bytes.
        assert_eq!(
            transaction.outputs,
            vec![pay(&address, 150_000), pay(&address, 49_791)]
        );
    }

    #[test]
    fn normal_change_keeps_the_dust_boundary_and_donates_smaller_remainders() {
        let (address, script) = p2wpkh(7);
        // One P2WPKH input with two outputs costs 141 satoshis at 1 sat/vB.
        for (amount, change, fee) in [
            (99_565, Some(294), 141),
            (99_566, None, 434),
            (99_860, None, 140),
        ] {
            let available = vec![utxo([1; 32], 2, 100_000, &script)];
            let builder = Builder::new(
                vec![pay(&address, amount)],
                context(available, &address, 1_000),
            );
            let transaction = builder.build().unwrap();
            let mut expected = vec![pay(&address, amount)];
            expected.extend(change.map(|change| pay(&address, change)));
            assert_eq!(transaction.outputs, expected);
            let paid: u64 = transaction
                .outputs
                .iter()
                .map(|output| output.amount.units())
                .sum();
            assert_eq!(100_000 - paid, fee);
        }
    }

    #[test]
    fn validation_order_is_inputs_recipients_fee_duplicates_then_scripts() {
        let (address, script) = p2wpkh(7);
        let source = utxo([7; 32], 2, 100_000, &script);
        let mut duplicate = source.clone();
        duplicate.output.value = Satoshi::from_atomic(1);
        let bare = utxo([8; 32], 0, 1, &ScriptBuf::from_bytes(vec![0x51]));
        for (available, transfers, rate, message) in [
            (
                Vec::new(),
                Vec::new(),
                0,
                "Bitcoin transfer has no available UTXOs",
            ),
            (
                vec![bare.clone()],
                Vec::new(),
                0,
                "Bitcoin transfer has no recipients",
            ),
            (
                vec![source.clone(), duplicate.clone()],
                vec![pay(&address, 1_000)],
                0,
                "Bitcoin fee rate must be greater than zero",
            ),
            (
                vec![source.clone(), duplicate],
                vec![pay(&address, 1_000)],
                1_000,
                "Bitcoin transfer contains a duplicate UTXO",
            ),
            (
                vec![bare],
                vec![pay(&address, 1_000)],
                1_000,
                "Bitcoin wallet supports P2WPKH and P2TR inputs only",
            ),
        ] {
            let error = Builder::new(transfers, context(available, &address, rate))
                .build()
                .unwrap_err();
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn transaction_construction_rejects_invalid_and_wrong_network_addresses() {
        let (regtest, script) = p2wpkh(7);
        let invalid = Address::unchecked("invalid");
        let mainnet = Address::from_script(&script, Network::Mainnet).unwrap();
        for (recipient, change, message) in [
            (&invalid, &regtest, "invalid Bitcoin address:"),
            (&regtest, &invalid, "invalid Bitcoin address:"),
            (
                &mainnet,
                &regtest,
                "Bitcoin address is for the wrong network:",
            ),
            (
                &regtest,
                &mainnet,
                "Bitcoin address is for the wrong network:",
            ),
        ] {
            let available = vec![utxo([1; 32], 0, 100_000, &script)];
            let builder = Builder::new(
                vec![pay(recipient, 50_000)],
                context(available, change, 1_000),
            );
            let error = builder.build().unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidAddress);
            assert!(error.message.starts_with(message), "{}", error.message);
        }
    }

    #[test]
    fn input_totals_are_checked_for_overflow() {
        let (address, script) = p2wpkh(7);
        let available = vec![
            utxo([0; 32], 0, u64::MAX, &script),
            utxo([1; 32], 0, 1, &script),
        ];
        let builder = Builder::new(vec![pay(&address, 1_000)], context(available, &address, 1));
        let error = builder.draining().build().unwrap_err();
        assert_eq!(
            error.message,
            "Bitcoin selected input amount overflowed u64"
        );
    }

    #[test]
    fn signing_builds_first_and_keeps_its_errors() {
        let (address, _) = p2wpkh(7);
        let key = crate::native::KeyPair::import(crypto::SecretBytes::new([1; 32])).unwrap();
        let builder = Builder::new(vec![pay(&address, 1)], context(Vec::new(), &address, 1));
        let signer: &dyn Signer = &key.pair;
        let error = block_on(builder.sign(signer)).unwrap_err();
        assert_eq!(error.message, "Bitcoin transfer has no available UTXOs");
    }

    fn funds_group(byte: u8, id: u8, value: u64, amount: u64, recipient: &Address) -> Funds {
        let (change, script) = p2wpkh(byte);
        Funds {
            available: vec![utxo([id; 32], 0, value, &script)],
            transfers: vec![Transfer::new(
                recipient.clone(),
                Satoshi::from_atomic(amount),
            )],
            change,
            sweep: None,
        }
    }

    #[test]
    fn grouped_sweep_receives_its_residual_after_fixed_payments_and_fee_share() {
        let (recipient, _) = p2wpkh(9);
        let (swept, _) = p2wpkh(8);
        let mut first = funds_group(1, 1, 100_000, 1_000, &recipient);
        first.sweep = Some(swept.clone());
        let second = funds_group(2, 2, 100_000, 2_000, &recipient);
        let built = combine(vec![first, second]).unwrap();
        assert_eq!(built.inputs.len(), 2);
        assert_eq!(
            built
                .outputs
                .iter()
                .map(|output| output.amount.units())
                .collect::<Vec<_>>(),
            [1_000, 2_000, 98_729, 98_000]
        );
        assert_eq!(built.outputs[2].to, swept);
    }

    fn combine(groups: Vec<Funds>) -> Result<UnsignedTransaction, Error> {
        Builder::grouped(Network::Regtest, groups, FeeRate::from_sat_per_kvb(1_000))
    }

    #[test]
    fn grouped_funding_preserves_each_sources_change() {
        let (recipient, _) = p2wpkh(3);
        let groups = vec![
            funds_group(1, 1, 100_000, 40_000, &recipient),
            funds_group(2, 2, 100_000, 30_000, &recipient),
        ];
        let changes: Vec<_> = groups.iter().map(|group| group.change.clone()).collect();
        let transaction = combine(groups).unwrap();
        assert_eq!(transaction.inputs.len(), 2);
        assert_eq!(transaction.outputs.len(), 4);
        for change in changes {
            assert!(transaction.outputs.iter().any(|output| output.to == change));
        }
    }

    #[test]
    fn grouped_funding_does_not_cross_subsidize_sources() {
        let (recipient, _) = p2wpkh(3);
        let groups = vec![
            funds_group(1, 1, 10_000, 20_000, &recipient),
            funds_group(2, 2, 1_000_000, 1_000, &recipient),
        ];
        let error = combine(groups).unwrap_err();
        assert_eq!(error.kind, ErrorKind::InsufficientFunds);
        assert_eq!(
            error.message,
            "a Bitcoin grouped source cannot fund its requested outputs"
        );
    }

    #[test]
    fn grouped_change_stays_disabled_after_fee_recalculation_and_preserves_source_order() {
        let (recipient, _) = p2wpkh(9);
        // Two inputs and four outputs cost 271 satoshis. Dropping the first
        // change lowers that estimate to 240, but must not reactivate it.
        for (first_amount, first_change, fee) in [(99_435, Some(294), 271), (99_440, None, 560)] {
            let first = funds_group(7, 2, 100_000, first_amount, &recipient);
            let second = funds_group(8, 1, 100_000, 99_000, &recipient);
            let (first_address, second_address) = (first.change.clone(), second.change.clone());
            let transaction = combine(vec![first, second]).unwrap();
            let order: Vec<_> = transaction
                .inputs
                .iter()
                .map(|spend| spend.utxo.outpoint.transaction_id.0)
                .collect();
            assert_eq!(order, [[2; 32], [1; 32]]);
            let mut expected = vec![
                Transfer::new(recipient.clone(), Satoshi::from_atomic(first_amount)),
                Transfer::new(recipient.clone(), Satoshi::from_atomic(99_000)),
            ];
            expected.extend(
                first_change.map(|change| {
                    Transfer::new(first_address.clone(), Satoshi::from_atomic(change))
                }),
            );
            expected.push(Transfer::new(second_address, Satoshi::from_atomic(1_000)));
            assert_eq!(transaction.outputs, expected);
            let paid: u64 = transaction
                .outputs
                .iter()
                .map(|output| output.amount.units())
                .sum();
            assert_eq!(200_000 - paid, fee);
        }
    }

    #[test]
    fn duplicates_are_checked_after_source_solvency() {
        let (address, script) = p2wpkh(7);
        let one = Funds {
            available: vec![utxo([7; 32], 2, 100_000, &script)],
            transfers: vec![Transfer::new(address.clone(), Satoshi::from_atomic(1_000))],
            change: address.clone(),
            sweep: None,
        };
        let mut distinct = one.clone();
        distinct.available[0].outpoint.output_index = 3;
        assert_eq!(
            combine(vec![one.clone(), distinct]).unwrap().inputs.len(),
            2
        );
        for (amount, message) in [
            (1_000, "Bitcoin transfer contains a duplicate UTXO"),
            (
                200_000,
                "a Bitcoin grouped source cannot fund its requested outputs",
            ),
        ] {
            let mut second = one.clone();
            second.transfers[0].amount = Satoshi::from_atomic(amount);
            assert_eq!(
                combine(vec![one.clone(), second]).unwrap_err().message,
                message
            );
        }
    }

    #[test]
    fn fees_are_charged_to_sources_in_order() {
        assert_eq!(Shares::charge(&[4, 0, 9, 7], 10).unwrap(), [0, 0, 3, 7]);
        assert_eq!(Shares::charge(&[7, 0, 9, 4], 10).unwrap(), [0, 0, 6, 4]);
        assert_eq!(Shares::charge(&[5, 0, 9], 0).unwrap(), [5, 0, 9]);
        assert_eq!(Shares::charge(&[5, 0, 9], 14).unwrap(), [0, 0, 0]);
        assert_eq!(
            Shares::charge(&[u64::MAX, u64::MAX], u64::MAX).unwrap(),
            [0, u64::MAX]
        );
        assert_eq!(Shares::charge(&[1, u64::MAX], u64::MAX).unwrap(), [0, 1]);
        for (surplus, fee) in [(&[5, 0, 9][..], 15), (&[][..], 1)] {
            let error = Shares::charge(surplus, fee).unwrap_err();
            assert_eq!(error.kind, ErrorKind::InsufficientFunds);
            assert_eq!(
                error.message,
                "Bitcoin grouped sources cannot cover the network fee"
            );
        }
    }
}

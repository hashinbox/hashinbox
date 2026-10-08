use base::Signer;
use bitcoin::{
    Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    transaction::Version,
};

use super::{SighashType, SignedTransaction, Transfer};
use crate::native::{Error, Network, block::Utxo};

/// One input of an unsigned transaction: the output it spends and its sequence.
///
/// The output is always present, which is what separates this from a block's
/// [`crate::native::block::Input`]: that one has no previous output for a
/// coinbase input or for a transaction read outside a block, and neither can
/// be signed, because the signature commits to the spent value and script.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Spend {
    /// The spent output.
    pub utxo: Utxo,
    /// The consensus sequence number (RBF-enabled by default).
    pub sequence: u32,
}

/// A built transaction before signing: inputs, outputs, and the sighash mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction {
    /// Consensus version (2).
    pub version: i32,
    /// Consensus lock time (0).
    pub lock_time: u32,
    /// Inputs in signing order.
    pub inputs: Vec<Spend>,
    /// Outputs in order, including change.
    pub outputs: Vec<Transfer>,
    /// The sighash every input signs with.
    pub sighash_type: SighashType,
}

impl UnsignedTransaction {
    /// Version 2, no lock time, RBF-enabled inputs, `SIGHASH_ALL`.
    pub(super) fn from_selected(utxos: Vec<Utxo>, outputs: Vec<Transfer>) -> Self {
        let sequence = Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32();
        Self {
            version: 2,
            lock_time: 0,
            inputs: utxos
                .into_iter()
                .map(|utxo| Spend { utxo, sequence })
                .collect(),
            outputs,
            sighash_type: SighashType::All,
        }
    }

    /// The fee these bytes pay: what the spent outputs hold minus what the
    /// outputs, change included, pay out.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidTransaction`] when either side
    /// overflows or the outputs exceed the inputs.
    pub fn fee(&self) -> Result<crate::native::Satoshi, Error> {
        let overflow = || Error::invalid_transaction("Bitcoin transaction value overflowed");
        let spent = self.inputs.iter().try_fold(
            crate::native::Satoshi::from_atomic(0),
            |total, input| {
                total
                    .checked_add(input.utxo.output.value)
                    .ok_or_else(overflow)
            },
        )?;
        let paid = self
            .outputs
            .iter()
            .try_fold(crate::native::Satoshi::from_atomic(0), |total, output| {
                total.checked_add(output.amount).ok_or_else(overflow)
            })?;
        spent.checked_sub(paid).ok_or_else(|| {
            Error::invalid_transaction("Bitcoin transaction outputs exceed its inputs")
        })
    }

    /// Signs every input with `signer`.
    ///
    /// # Errors
    ///
    /// When an input is not P2WPKH or P2TR, a sighash cannot be computed, or
    /// the signer fails or returns a signature that does not verify.
    pub async fn sign(
        self,
        network: Network,
        signer: &dyn Signer,
    ) -> Result<SignedTransaction, Error> {
        let signers = vec![signer; self.inputs.len()];
        self.sign_each(network, &signers).await
    }

    /// The consensus transaction with empty witnesses.
    pub(super) fn native(&self, network: Network) -> Result<Transaction, Error> {
        let input = self
            .inputs
            .iter()
            .map(|spend| TxIn {
                previous_output: bitcoin::OutPoint::new(
                    Txid::from(spend.utxo.outpoint.transaction_id),
                    spend.utxo.outpoint.output_index,
                ),
                script_sig: ScriptBuf::new(),
                sequence: Sequence(spend.sequence),
                witness: Witness::new(),
            })
            .collect();
        let output = self
            .outputs
            .iter()
            .map(|transfer| {
                Ok(TxOut {
                    value: Amount::from_sat(transfer.amount.units()),
                    script_pubkey: transfer.to.script_pubkey(network)?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(Transaction {
            version: Version(self.version),
            lock_time: absolute::LockTime::from_consensus(self.lock_time),
            input,
            output,
        })
    }
}

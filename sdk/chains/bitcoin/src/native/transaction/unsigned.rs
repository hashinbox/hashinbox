use base::Signer;
use bitcoin::{
    Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness, absolute,
    transaction::Version,
};

use super::{SighashType, SignedTransaction, Transfer};
use crate::native::{Error, Network, block::Utxo};

/// One input of an unsigned transaction: the output it spends and its sequence.
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

#[cfg(test)]
mod tests {
    use bitcoin::OutPoint;

    use super::*;
    use crate::native::{
        Address, ErrorKind, Satoshi, TransactionId,
        block::{Outpoint, Output},
    };

    fn utxo(byte: u8, index: u32, value: u64) -> Utxo {
        Utxo {
            outpoint: Outpoint {
                transaction_id: TransactionId([byte; 32]),
                output_index: index,
            },
            output: Output {
                value: Satoshi::from_atomic(value),
                script_pubkey: vec![0x51],
                address: None,
            },
        }
    }

    #[test]
    fn selected_funding_keeps_order_and_transaction_defaults() {
        let sources = [utxo(7, 7, 7), utxo(3, 3, 3)];
        let address = Address::unchecked("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn");
        let outputs =
            [11, 5].map(|value| Transfer::new(address.clone(), Satoshi::from_atomic(value)));
        let unsigned = UnsignedTransaction::from_selected(sources.to_vec(), outputs.to_vec());
        assert_eq!((unsigned.version, unsigned.lock_time), (2, 0));
        assert_eq!(unsigned.sighash_type, SighashType::All);
        assert_eq!(unsigned.outputs, outputs);
        for (spend, source) in unsigned.inputs.iter().zip(&sources) {
            assert_eq!(&spend.utxo, source);
            assert_eq!(
                spend.sequence,
                Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32()
            );
        }
    }

    #[test]
    fn native_conversion_keeps_fields_and_empty_signing_material() {
        let address = Address::unchecked("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn");
        let unsigned = UnsignedTransaction {
            version: -4,
            lock_time: u32::MAX,
            inputs: vec![Spend {
                utxo: utxo(7, u32::MAX, 42),
                sequence: 123,
            }],
            outputs: vec![Transfer::new(
                address.clone(),
                Satoshi::from_atomic(u64::MAX),
            )],
            sighash_type: SighashType::All,
        };
        let native = unsigned.native(Network::Regtest).unwrap();
        assert_eq!(native.version, Version(-4));
        assert_eq!(native.lock_time.to_consensus_u32(), u32::MAX);
        assert_eq!(
            native.input[0].previous_output,
            OutPoint::new(Txid::from(TransactionId([7; 32])), u32::MAX)
        );
        assert_eq!(native.input[0].sequence, Sequence(123));
        assert!(native.input[0].script_sig.is_empty() && native.input[0].witness.is_empty());
        assert_eq!(
            native.output,
            [TxOut {
                value: Amount::from_sat(u64::MAX),
                script_pubkey: address.script_pubkey(Network::Regtest).unwrap(),
            }]
        );
    }

    #[test]
    fn native_conversion_keeps_output_network_rejection() {
        let address = Address::unchecked("1BitcoinEaterAddressDontSendf59kuE");
        let expected = address.script_pubkey(Network::Regtest).unwrap_err();
        let unsigned = UnsignedTransaction {
            version: 2,
            lock_time: 0,
            inputs: Vec::new(),
            outputs: vec![Transfer::new(address, Satoshi::from_atomic(1_000))],
            sighash_type: SighashType::All,
        };
        assert_eq!(expected.kind, ErrorKind::InvalidAddress);
        assert_eq!(unsigned.native(Network::Regtest), Err(expected));
    }
}

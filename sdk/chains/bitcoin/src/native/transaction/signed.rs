use base::transaction::Envelope;
use bitcoin::{Transaction as NativeTransaction, consensus, hex::DisplayHex};

use super::TransactionId;
use crate::native::{Error, Network, block::Transaction};

/// A signed transaction ready for submission: its id and exact wire bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedTransaction {
    /// The txid of `envelope`.
    pub id: TransactionId,
    /// Exact consensus bytes, witnesses included; `id` is their txid.
    pub envelope: Envelope,
}

impl SignedTransaction {
    /// Accepts `envelope` only when it decodes as one Bitcoin transaction
    /// whose txid is `id`.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidTransaction`] for undecodable bytes
    /// or another txid.
    pub fn from_envelope(id: TransactionId, envelope: Envelope) -> Result<Self, Error> {
        let signed = Self { id, envelope };
        signed.decode()?;
        Ok(signed)
    }

    /// BIP141 virtual size.
    ///
    /// # Errors
    ///
    /// When the retained bytes no longer decode to this txid.
    pub fn virtual_size(&self) -> Result<u64, Error> {
        u64::try_from(self.decode()?.vsize())
            .map_err(|_| Error::invalid_transaction("signed Bitcoin virtual size exceeds u64"))
    }

    /// The transaction's inputs and outputs, with addresses for `network`.
    ///
    /// Previous outputs are unknown here, so every `Input::previous` and the
    /// fee are `None`: a caller that needs the fee must compare the inputs
    /// with its own reserved previous outputs.
    ///
    /// # Errors
    ///
    /// When the retained bytes no longer decode to this txid.
    pub fn transaction(&self, network: Network) -> Result<Transaction, Error> {
        Ok(Transaction::unconfirmed(&self.decode()?, network))
    }

    /// The envelope as Bitcoin Core's lowercase hexadecimal argument.
    pub(crate) fn hex(&self) -> String {
        self.envelope.as_bytes().to_lower_hex_string()
    }

    fn decode(&self) -> Result<NativeTransaction, Error> {
        let transaction: NativeTransaction = consensus::deserialize(self.envelope.as_bytes())
            .map_err(|error| {
                Error::invalid_transaction(format!(
                    "could not decode signed Bitcoin consensus bytes: {error}"
                ))
            })?;
        let computed = TransactionId::from(transaction.compute_txid());
        if computed != self.id {
            return Err(Error::invalid_transaction(format!(
                "signed Bitcoin transaction ID mismatch: expected {}, computed {computed}",
                self.id
            )));
        }
        Ok(transaction)
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Txid, Witness, absolute, hashes::Hash,
        transaction::Version,
    };

    use super::*;
    use crate::native::{ErrorKind, Satoshi, block::Outpoint};

    fn transaction() -> NativeTransaction {
        NativeTransaction {
            version: Version::TWO,
            lock_time: absolute::LockTime::from_consensus(500_000),
            input: vec![
                TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([7; 32]), 3),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::from_slice(&[b"signed-witness"]),
                },
                TxIn {
                    previous_output: OutPoint::new(Txid::from_byte_array([8; 32]), 9),
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::MAX,
                    witness: Witness::new(),
                },
            ],
            output: vec![
                TxOut {
                    value: Amount::from_sat(42_000),
                    script_pubkey: ScriptBuf::from_bytes(vec![0x51]),
                },
                TxOut {
                    value: Amount::from_sat(7_000),
                    script_pubkey: ScriptBuf::from_bytes(vec![0x00, 0x14, 1, 2, 3]),
                },
            ],
        }
    }

    fn signed(transaction: &NativeTransaction) -> SignedTransaction {
        SignedTransaction::from_envelope(
            TransactionId::from(transaction.compute_txid()),
            Envelope::new(consensus::serialize(transaction)),
        )
        .unwrap()
    }

    #[test]
    fn signed_transaction_preserves_verified_exact_bytes_and_redacts_them() {
        let transaction = transaction();
        let signed = signed(&transaction);
        assert_eq!(signed.id, TransactionId::from(transaction.compute_txid()));
        assert_eq!(
            signed.envelope.as_bytes(),
            consensus::serialize(&transaction)
        );
        assert!(!format!("{signed:?}").contains("signed-witness"));
        assert_eq!(
            signed.virtual_size().unwrap(),
            u64::try_from(transaction.vsize()).unwrap()
        );
    }

    #[test]
    fn envelopes_must_decode_to_their_transaction_id() {
        let mut trailing = consensus::serialize(&transaction());
        trailing.push(0);
        for bytes in [vec![0xff], trailing] {
            let error =
                SignedTransaction::from_envelope(TransactionId([0; 32]), Envelope::new(bytes))
                    .unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidTransaction);
            assert!(error.message.contains("could not decode"));
        }
        let transaction = transaction();
        let computed = TransactionId::from(transaction.compute_txid());
        let error = SignedTransaction::from_envelope(
            TransactionId([0; 32]),
            Envelope::new(consensus::serialize(&transaction)),
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "signed Bitcoin transaction ID mismatch: expected {}, computed {computed}",
                TransactionId([0; 32])
            )
        );
    }

    #[test]
    fn transaction_view_keeps_consensus_order_without_previous_outputs() {
        let signed = signed(&transaction());
        let view = signed.transaction(Network::Regtest).unwrap();
        assert_eq!(view.id(), &signed.id);
        assert_eq!(view.fee(), None);
        assert_eq!(
            view.inputs
                .iter()
                .map(|input| (input.outpoint, input.sequence, input.previous.is_none()))
                .collect::<Vec<_>>(),
            vec![
                (
                    Outpoint {
                        transaction_id: TransactionId([7; 32]),
                        output_index: 3,
                    },
                    Sequence::ENABLE_RBF_NO_LOCKTIME.to_consensus_u32(),
                    true,
                ),
                (
                    Outpoint {
                        transaction_id: TransactionId([8; 32]),
                        output_index: 9,
                    },
                    Sequence::MAX.to_consensus_u32(),
                    true,
                ),
            ]
        );
        assert_eq!(
            view.outputs
                .iter()
                .map(|output| (output.value, output.script_pubkey.clone()))
                .collect::<Vec<_>>(),
            vec![
                (Satoshi::from_atomic(42_000), vec![0x51]),
                (Satoshi::from_atomic(7_000), vec![0x00, 0x14, 1, 2, 3]),
            ]
        );
    }
}

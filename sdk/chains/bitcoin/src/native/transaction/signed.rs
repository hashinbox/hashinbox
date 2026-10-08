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

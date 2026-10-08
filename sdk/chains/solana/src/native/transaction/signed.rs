use std::fmt;

use base::{
    PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme, Signer,
    transaction::Envelope,
};
use solana_signature::Signature;
use solana_transaction::Transaction;

use super::{Lifetime, TransactionId, UnsignedTransaction};
use crate::native::{Address, Error, ErrorKind};

/// One signed transfer: the exact wire bytes and what submission and
/// recovery need to know about them.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedTransaction {
    /// The first signature.
    pub id: TransactionId,
    /// Exact serialized transaction bytes, submitted unchanged.
    pub envelope: Envelope,
    /// The sender, who signed and pays the fee.
    pub from: Address,
    /// Context slot of the reads the transaction was built from; later reads
    /// about it must not be older.
    pub min_slot: u64,
    /// The blockhash it references and the height it expires after.
    pub lifetime: Lifetime,
}

impl fmt::Debug for SignedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SignedTransaction")
            .field("id", &self.id)
            .field("envelope", &"[REDACTED]")
            .finish()
    }
}

impl UnsignedTransaction {
    /// Signs this message with `signer`, which must control `from`, the
    /// message's fee payer, and binds it to `lifetime` and `min_slot`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Signer`] when `from`, the message, and `lifetime` do not
    /// agree, the signer fails, or its signature does not verify for `from`;
    /// [`ErrorKind::Other`] when encoding fails.
    pub async fn sign(
        self,
        from: &Address,
        lifetime: Lifetime,
        min_slot: u64,
        signer: &dyn Signer,
    ) -> Result<SignedTransaction, Error> {
        if self.payer().as_ref() != Some(from)
            || &self.native().recent_blockhash != lifetime.blockhash()
        {
            return Err(Error::new(
                ErrorKind::Signer,
                "Solana envelope source, message, and lifetime must agree",
            ));
        }
        let message = self.bytes()?;
        let signature = Self::signature(signer, from, message).await?;
        let transaction = Transaction {
            signatures: vec![signature],
            message: self.native().clone(),
        };
        transaction.verify().map_err(|_| {
            Error::new(
                ErrorKind::Signer,
                "Solana signed transaction did not verify locally",
            )
        })?;
        let bytes = bincode::serialize(&transaction).map_err(|_| {
            Error::new(
                ErrorKind::Other,
                "Solana signed transaction encoding failed",
            )
        })?;
        Ok(SignedTransaction {
            id: signature.into(),
            envelope: Envelope::new(bytes),
            from: from.clone(),
            min_slot,
            lifetime,
        })
    }

    /// An Ed25519 signature of the exact `message` that verifies for `from`.
    async fn signature(
        signer: &dyn Signer,
        from: &Address,
        message: Vec<u8>,
    ) -> Result<Signature, Error> {
        let mismatch = || {
            Error::new(
                ErrorKind::Signer,
                "Solana signature does not match the source and message",
            )
        };
        let signed = signer
            .sign(SignRequest {
                payload: SignablePayload::Message(message.clone()),
                scheme: SignatureScheme::Ed25519,
                encoding: SignatureEncoding::Raw,
                public_key_format: PublicKeyFormat::Raw,
                key_tweak: None,
            })
            .await
            .map_err(|_| Error::new(ErrorKind::Signer, "Solana signing failed"))?;
        if signed.public_key.bytes != from.as_bytes() {
            return Err(mismatch());
        }
        let signature =
            Signature::try_from(signed.signature.bytes.as_slice()).map_err(|_| mismatch())?;
        if !signature.verify(from.as_bytes(), &message) {
            return Err(mismatch());
        }
        Ok(signature)
    }
}

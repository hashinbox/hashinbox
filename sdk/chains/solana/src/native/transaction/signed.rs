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

#[cfg(test)]
mod tests {
    use futures_executor::block_on;
    use solana_hash::Hash;

    use super::*;
    use crate::native::{KeyPair, Lamport, Memo, Seed, transaction::Transfer};

    fn key(value: u8) -> KeyPair {
        let seed = hex::encode([value; 32])
            .parse::<Seed>()
            .expect("fixture seed");
        KeyPair::import(seed.into()).expect("fixture key")
    }

    fn lifetime(byte: u8) -> Lifetime {
        Lifetime::new(Hash::new_from_array([byte; 32]), 44)
    }

    fn message(from: &Address, lifetime: &Lifetime) -> UnsignedTransaction {
        let transfer = Transfer {
            to: Address::from_bytes([8; 32]),
            amount: Lamport::from_atomic(17),
        };
        UnsignedTransaction::new(
            from,
            &transfer,
            Memo::from_bytes([3; Memo::LENGTH]),
            lifetime,
        )
    }

    #[test]
    fn binds_sender_signature_bytes_floor_and_lifetime() {
        let key = key(7);
        let from = key.address().clone();
        let signed = block_on(message(&from, &lifetime(9)).sign(&from, lifetime(9), 11, &key))
            .expect("signed transaction");

        assert_eq!(signed.from, from);
        assert_eq!(signed.min_slot, 11);
        assert_eq!(signed.lifetime.last_valid_block_height(), 44);
        assert!(!signed.envelope.as_bytes().is_empty());
        let debug = format!("{signed:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(&hex::encode(signed.envelope.as_bytes())));
    }

    #[test]
    fn rejects_a_signer_that_does_not_control_the_sender() {
        let from = key(7).address().clone();
        let error = block_on(message(&from, &lifetime(9)).sign(&from, lifetime(9), 11, &key(6)))
            .unwrap_err();
        assert_eq!(
            (error.kind, error.message.as_str()),
            (
                ErrorKind::Signer,
                "Solana signature does not match the source and message"
            )
        );
    }

    #[test]
    fn rejects_message_payer_and_blockhash_mismatch_with_binding_error() {
        let signer = key(7);
        for (payer, signed_lifetime) in [(key(8), lifetime(9)), (key(7), lifetime(10))] {
            let error = block_on(message(payer.address(), &lifetime(9)).sign(
                signer.address(),
                signed_lifetime,
                11,
                &signer,
            ))
            .unwrap_err();
            assert_eq!(
                (error.kind, error.message.as_str()),
                (
                    ErrorKind::Signer,
                    "Solana envelope source, message, and lifetime must agree"
                )
            );
        }
    }

    #[test]
    fn signed_bytes_match_independently_signed_native_transaction() {
        use solana_keypair::{Keypair, Signer as _};

        let signer = key(7);
        let from = signer.address().clone();
        let unsigned = message(&from, &lifetime(9));
        let native_key = Keypair::new_from_array([7; 32]);
        let signature = native_key.sign_message(&unsigned.native().serialize());
        let expected = bincode::serialize(&Transaction {
            signatures: vec![signature],
            message: unsigned.native().clone(),
        })
        .unwrap();
        let signed = block_on(unsigned.sign(&from, lifetime(9), u64::MAX, &signer)).unwrap();
        assert_eq!(signed.envelope.as_bytes(), expected);
        assert_eq!(signed.id, TransactionId::from(signature));
        assert_eq!(signed.id.to_string(), signature.to_string());
        assert_eq!(signed.min_slot, u64::MAX);
    }
}

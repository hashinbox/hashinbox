use std::fmt;

use base::{TransactionId, transaction::Envelope as SignedBytes};
use solana_signature::Signature;
use solana_transaction::Transaction;

use crate::{Address, BlockhashLifetime, Error, ErrorKind, Key};

use super::Message;

/// Immutable Solana submission evidence for one authored occurrence.
#[derive(Clone, PartialEq, Eq)]
pub struct Envelope {
    source: Address,
    index: usize,
    message: Message,
    signature: Signature,
    id: TransactionId,
    signed: SignedBytes,
    floor: u64,
    lifetime: BlockhashLifetime,
}

impl Envelope {
    pub fn sign(
        source: Address,
        index: usize,
        message: Message,
        floor: u64,
        lifetime: BlockhashLifetime,
        key: &Key,
    ) -> Result<Self, Error> {
        message.validate_binding(&source, &lifetime, key)?;
        let message_bytes = message.wire_bytes()?;
        let signed = key.sign_message(&message_bytes)?;
        let signature = Signature::from(*signed.signature());
        let transaction = Transaction {
            signatures: vec![signature],
            message: message.native().clone(),
        };
        transaction.verify().map_err(|_| {
            Error::new(
                ErrorKind::Signing,
                "Solana signed transaction did not verify locally",
            )
        })?;
        let bytes = bincode::serialize(&transaction).map_err(|_| {
            Error::new(
                ErrorKind::Signing,
                "Solana signed transaction encoding failed",
            )
        })?;
        Ok(Self {
            source,
            index,
            message,
            signature,
            id: signed.id().clone(),
            signed: SignedBytes::new(bytes),
            floor,
            lifetime,
        })
    }

    #[must_use]
    pub const fn source(&self) -> &Address {
        &self.source
    }

    #[must_use]
    pub const fn index(&self) -> usize {
        self.index
    }

    #[must_use]
    pub const fn message(&self) -> &Message {
        &self.message
    }

    #[must_use]
    pub const fn signature(&self) -> &Signature {
        &self.signature
    }

    #[must_use]
    pub const fn id(&self) -> &TransactionId {
        &self.id
    }

    #[must_use]
    pub fn signed_bytes(&self) -> &[u8] {
        self.signed.as_bytes()
    }

    #[must_use]
    pub const fn floor(&self) -> u64 {
        self.floor
    }

    #[must_use]
    pub const fn lifetime(&self) -> &BlockhashLifetime {
        &self.lifetime
    }
}

impl Message {
    fn validate_binding(
        &self,
        source: &Address,
        lifetime: &BlockhashLifetime,
        key: &Key,
    ) -> Result<(), Error> {
        if key.address() != source
            || self.native().account_keys.first().map(|key| key.to_bytes())
                != Some(*source.as_bytes())
            || &self.native().recent_blockhash != lifetime.blockhash()
        {
            return Err(Error::new(
                ErrorKind::Signing,
                "Solana envelope source, message, and lifetime must agree",
            ));
        }
        Ok(())
    }
}

impl fmt::Debug for Envelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Envelope")
            .field("source", &self.source)
            .field("index", &self.index)
            .field("id", &self.id)
            .field("signed", &self.signed)
            .field("floor", &self.floor)
            .field("lifetime", &self.lifetime)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use solana_hash::Hash;

    use crate::{Lamport, Memo, Seed};

    use super::*;

    fn key(value: u8) -> Key {
        Key::from_seed(
            hex::encode([value; 32])
                .parse::<Seed>()
                .expect("fixture seed"),
        )
        .expect("fixture key")
    }

    #[test]
    fn binds_source_occurrence_message_signature_bytes_floor_and_lifetime() {
        let key = key(7);
        let source = key.address().clone();
        let lifetime = BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let message = Message::native_transfer(
            &source,
            &Address::from_bytes([8; 32]),
            Lamport::from_atomic(17),
            Memo::from_bytes([3; Memo::LENGTH]),
            &lifetime,
        )
        .expect("message");
        let envelope = Envelope::sign(source.clone(), 5, message, 11, lifetime, &key)
            .expect("immutable envelope");

        assert_eq!(envelope.source(), &source);
        assert_eq!(envelope.index(), 5);
        assert_eq!(envelope.floor(), 11);
        assert_eq!(envelope.lifetime().last_valid_block_height(), 44);
        assert_eq!(envelope.id().as_str(), envelope.signature().to_string());
        assert!(!envelope.signed_bytes().is_empty());
        let debug = format!("{envelope:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains(&hex::encode(envelope.signed_bytes())));
    }

    #[test]
    fn rejects_a_key_that_does_not_own_the_source() {
        let owner = key(7);
        let wrong = key(6);
        let source = owner.address().clone();
        let lifetime = BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let message = Message::native_transfer(
            &source,
            &Address::from_bytes([8; 32]),
            Lamport::from_atomic(17),
            Memo::from_bytes([3; Memo::LENGTH]),
            &lifetime,
        )
        .expect("message");

        assert_eq!(
            Envelope::sign(source, 0, message, 11, lifetime, &wrong)
                .expect_err("wrong signer")
                .kind(),
            ErrorKind::Signing
        );
    }

    #[test]
    fn rejects_message_payer_and_blockhash_mismatch_with_binding_error() {
        let signer = key(7);
        let other = key(8);
        let lifetime = BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let wrong_lifetime = BlockhashLifetime::new(Hash::new_from_array([10; 32]), 44);
        for (payer, signed_lifetime) in [
            (other.address(), &lifetime),
            (signer.address(), &wrong_lifetime),
        ] {
            let message = Message::native_transfer(
                payer,
                &Address::from_bytes([11; 32]),
                Lamport::from_atomic(17),
                Memo::from_bytes([3; Memo::LENGTH]),
                &lifetime,
            )
            .unwrap();
            let error = Envelope::sign(
                signer.address().clone(),
                0,
                message,
                11,
                signed_lifetime.clone(),
                &signer,
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Signing);
            assert_eq!(
                error.to_string(),
                "Solana envelope source, message, and lifetime must agree"
            );
        }
    }

    #[test]
    fn signed_bytes_match_independently_signed_native_transaction() {
        use solana_keypair::{Keypair, Signer as _};

        let signer = key(7);
        let lifetime = BlockhashLifetime::new(Hash::new_from_array([9; 32]), 44);
        let message = Message::native_transfer(
            signer.address(),
            &Address::from_bytes([8; 32]),
            Lamport::from_atomic(17),
            Memo::from_bytes([3; Memo::LENGTH]),
            &lifetime,
        )
        .unwrap();
        let native_key = Keypair::new_from_array([7; 32]);
        let signature = native_key.sign_message(&message.native().serialize());
        let transaction = Transaction {
            signatures: vec![signature],
            message: message.native().clone(),
        };
        let expected = bincode::serialize(&transaction).unwrap();
        let envelope = Envelope::sign(
            signer.address().clone(),
            2,
            message,
            u64::MAX,
            lifetime,
            &signer,
        )
        .unwrap();
        assert_eq!(envelope.signed_bytes(), expected);
        assert_eq!(envelope.signature(), &signature);
        assert_eq!(envelope.id().as_str(), signature.to_string());
        assert_eq!(envelope.floor(), u64::MAX);
    }
}

use std::fmt;

use alloy_consensus::TxEnvelope;
use alloy_eips::Decodable2718;
use alloy_primitives::keccak256;
use base::transaction::Envelope;

use super::TransactionId;
use crate::native::{Error, ErrorKind, Wei};

/// A signed transaction ready for submission: its id and exact wire bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct SignedTransaction {
    /// Keccak-256 of `envelope`.
    pub id: TransactionId,
    /// Exact EIP-2718 bytes; `id` is their Keccak-256.
    pub envelope: Envelope,
}

impl SignedTransaction {
    /// Reconstructs a signed transaction only when the caller-provided ID is
    /// the canonical Keccak-256 hash of the exact opaque envelope bytes.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] for empty bytes or a hash mismatch.
    pub fn from_envelope(id: TransactionId, envelope: Envelope) -> Result<Self, Error> {
        if envelope.as_bytes().is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "signed Ethereum envelope must not be empty",
            ));
        }
        if keccak256(envelope.as_bytes()).0 != *id.as_bytes() {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "signed Ethereum envelope hash does not match its transaction ID",
            ));
        }
        Ok(Self { id, envelope })
    }

    /// Decodes the exact EIP-2718 envelope and exposes only the EIP-1559 fee
    /// authorization fields needed by a caller-owned policy boundary.
    ///
    /// The opaque envelope and signature are deliberately absent from the
    /// returned value and from all inspection errors.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidTransaction`] naming the first violated rule.
    pub fn inspect_eip1559_fees(&self) -> Result<FeeInspection, Error> {
        let invalid = |message: &str| Error::new(ErrorKind::InvalidTransaction, message);
        let envelope = TxEnvelope::decode_2718_exact(self.envelope.as_bytes())
            .map_err(|_| invalid("signed Ethereum envelope is malformed"))?;
        let signed = envelope
            .as_eip1559()
            .ok_or_else(|| invalid("signed Ethereum envelope is not an EIP-1559 transaction"))?;
        let transaction = signed.tx();
        if transaction.chain_id == 0 {
            return Err(invalid("signed EIP-1559 transaction has a zero chain ID"));
        }
        if transaction.gas_limit == 0 {
            return Err(invalid("signed EIP-1559 transaction has a zero gas limit"));
        }
        if transaction.max_priority_fee_per_gas > transaction.max_fee_per_gas {
            return Err(invalid(
                "signed EIP-1559 priority fee exceeds its maximum fee per gas",
            ));
        }

        let max_fee_per_gas = Wei::from_u128(transaction.max_fee_per_gas);
        let maximum_total_fee = max_fee_per_gas
            .checked_mul_u64(transaction.gas_limit)
            .ok_or_else(|| {
                invalid("signed EIP-1559 maximum total fee overflows the Ethereum amount range")
            })?;
        Ok(FeeInspection {
            chain_id: transaction.chain_id,
            gas_limit: transaction.gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas: Wei::from_u128(transaction.max_priority_fee_per_gas),
            maximum_total_fee,
        })
    }
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

/// Policy-relevant fields decoded from one exact signed EIP-1559 envelope.
///
/// This boundary intentionally carries neither the opaque envelope nor its
/// signature and does not implement `Debug`.
///
/// It is `native`'s own fee-authorization surface, for a caller driving
/// `native` directly and for `tests/transaction_adapter.rs`, which asserts
/// that what the wallet signed matches its configured fee limits. The
/// The adapter's transfer builder reads one field of it, reporting
/// `maximum_total_fee` as `base::SignedTransaction::fee`, decoded from the
/// exact envelope rather than from the build context, so the figure is the
/// one that was signed.
#[derive(Clone, PartialEq, Eq)]
pub struct FeeInspection {
    /// The chain the envelope signs for.
    pub chain_id: u64,
    /// Signed gas limit.
    pub gas_limit: u64,
    /// Signed max fee per gas.
    pub max_fee_per_gas: Wei,
    /// Signed priority fee per gas.
    pub max_priority_fee_per_gas: Wei,
    /// `gas_limit * max_fee_per_gas`: the most this transaction can cost.
    pub maximum_total_fee: Wei,
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
    use alloy_eips::Encodable2718;
    use alloy_primitives::{Address, Bytes, Signature, U256};

    use super::*;

    fn signed(bytes: Vec<u8>) -> SignedTransaction {
        SignedTransaction::from_envelope(
            TransactionId::from_bytes(keccak256(&bytes).0),
            Envelope::new(bytes),
        )
        .expect("matching transaction ID must accept the bytes")
    }

    #[test]
    fn envelope_construction_validates_hash_and_redacts_debug() {
        let envelope = vec![0x02, 0x01, 0x02];
        assert!(!format!("{:?}", signed(envelope.clone())).contains("020102"));
        let error = SignedTransaction::from_envelope(
            TransactionId::from_bytes([0; 32]),
            Envelope::new(envelope),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::InvalidTransaction);
        assert_eq!(
            error.message,
            "signed Ethereum envelope hash does not match its transaction ID"
        );
    }

    #[test]
    fn fee_inspection_rejects_malformed_and_non_eip1559_envelopes() {
        let malformed = signed(b"hello".to_vec());
        assert_eq!(
            malformed.inspect_eip1559_fees().err().unwrap().message,
            "signed Ethereum envelope is malformed"
        );

        let legacy = TxLegacy {
            chain_id: Some(1),
            nonce: 1,
            gas_price: 2,
            gas_limit: 21_000,
            to: Address::ZERO.into(),
            value: U256::ZERO,
            input: Bytes::new(),
        };
        let signature =
            Signature::from_scalars_and_parity([1_u8; 32].into(), [2_u8; 32].into(), false);
        let envelope: TxEnvelope = legacy.into_signed(signature).into();
        let mut legacy_bytes = Vec::with_capacity(envelope.encode_2718_len());
        envelope.encode_2718(&mut legacy_bytes);
        assert_eq!(
            signed(legacy_bytes)
                .inspect_eip1559_fees()
                .err()
                .unwrap()
                .message,
            "signed Ethereum envelope is not an EIP-1559 transaction"
        );
    }
}

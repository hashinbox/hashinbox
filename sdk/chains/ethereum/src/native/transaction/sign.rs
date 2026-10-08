use super::{SignedTransaction, TransactionId, UnsignedTransaction};
use crate::native::{Error, ErrorKind};
use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, Bytes, Signature as AlloySignature, TxKind, keccak256};
use base::{
    Digest, PublicKeyFormat, SignRequest, SignablePayload, SignatureEncoding, SignatureScheme,
    Signer,
};

impl UnsignedTransaction {
    /// Signs the EIP-1559 digest and verifies the recovered sender.
    pub(super) async fn sign(self, signer: &dyn Signer) -> Result<SignedTransaction, Error> {
        let native = self.eip1559()?;
        let signature_hash = native.signature_hash();
        let signed = signer
            .sign(SignRequest {
                payload: SignablePayload::Digest(Digest {
                    bytes: signature_hash.to_vec(),
                }),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Recoverable,
                public_key_format: PublicKeyFormat::Raw,
                key_tweak: None,
            })
            .await
            .map_err(|error| {
                Error::new(
                    ErrorKind::Signer,
                    format!("Ethereum signing failed: {error}"),
                )
            })?;
        SignedTransaction::from_signature(native, &self.context.from, signed.signature)
    }

    fn eip1559(&self) -> Result<TxEip1559, Error> {
        let context = &self.context;
        let max_fee_per_gas = context.max_fee_per_gas.checked_to_u128().ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum max fee per gas exceeds u128",
            )
        })?;
        let max_priority_fee_per_gas = context
            .max_priority_fee_per_gas
            .checked_to_u128()
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidTransaction,
                    "Ethereum priority fee exceeds u128",
                )
            })?;

        Ok(TxEip1559 {
            chain_id: context.chain_id,
            nonce: context.nonce,
            gas_limit: context.gas_limit,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            to: self.to.as_ref().map_or(TxKind::Create, |address| {
                TxKind::Call(Address::from(*address.as_bytes()))
            }),
            value: self.value.u256(),
            access_list: Default::default(),
            input: Bytes::from(self.input.clone()),
        })
    }
}

impl SignedTransaction {
    fn from_signature(
        native: TxEip1559,
        sender: &crate::native::Address,
        signature: base::Signature,
    ) -> Result<Self, Error> {
        if signature.scheme != SignatureScheme::EcdsaSecp256k1
            || signature.encoding != SignatureEncoding::Recoverable
        {
            return Err(Error::new(
                ErrorKind::Signer,
                "Ethereum signer returned an incompatible signature",
            ));
        }
        let signature = AlloySignature::try_from(signature.bytes.as_slice()).map_err(|error| {
            Error::new(
                ErrorKind::Signer,
                format!("invalid recoverable Ethereum signature: {error}"),
            )
        })?;
        let signed = native.into_signed(signature);
        let recovered = signed.recover_signer().map_err(|error| {
            Error::new(
                ErrorKind::Signer,
                format!("could not recover Ethereum signer: {error}"),
            )
        })?;
        if recovered.into_array() != *sender.as_bytes() {
            return Err(Error::new(
                ErrorKind::Signer,
                "Ethereum signature does not match the transaction sender",
            ));
        }

        let envelope: TxEnvelope = signed.into();
        let mut encoded = Vec::with_capacity(envelope.encode_2718_len());
        envelope.encode_2718(&mut encoded);
        let id = TransactionId::from_bytes(keccak256(&encoded).0);

        Ok(Self {
            id,
            envelope: base::transaction::Envelope::new(encoded),
        })
    }
}

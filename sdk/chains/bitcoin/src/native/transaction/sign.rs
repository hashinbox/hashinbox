use base::{
    Digest, PublicKeyFormat, ScalarTweak, SignRequest, SignablePayload, SignatureEncoding,
    SignatureScheme, SignedPayload, Signer,
};
use bitcoin::{
    Address as NativeAddress, Amount, CompressedPublicKey, Transaction, TxOut, Witness, consensus,
    hashes::Hash,
    key::{TapTweak, XOnlyPublicKey},
    secp256k1::{Message, Secp256k1, ecdsa, schnorr},
    sighash::{
        EcdsaSighashType, Prevouts, SegwitV0Sighash, SighashCache, TapSighash, TapSighashType,
    },
};

use base::transaction::Envelope;

use super::{SighashType, SignedTransaction, Spend, TransactionId, UnsignedTransaction};
use crate::native::{Error, Network};

struct InputSigner<'a, S: ?Sized> {
    transaction: &'a Transaction,
    prevouts: &'a [TxOut],
    sighash_type: SighashType,
    network: Network,
    signer: &'a S,
}

impl UnsignedTransaction {
    /// Signs input `i` with `signers[i]`: one transaction funded by several
    /// independently owned outputs. Callers decide which wallets participate;
    /// this keeps input order and sighash rules.
    ///
    /// # Errors
    ///
    /// When `signers` does not have one entry per input, or as for [`Self::sign`].
    pub async fn sign_each<S: Signer + ?Sized>(
        self,
        network: Network,
        signers: &[&S],
    ) -> Result<SignedTransaction, Error> {
        if self.inputs.len() != signers.len() {
            return Err(Error::invalid_transaction(
                "Bitcoin transaction needs exactly one signer per input",
            ));
        }
        let mut native = self.native(network)?;
        let prevouts = self
            .inputs
            .iter()
            .map(|spend| TxOut {
                value: Amount::from_sat(spend.utxo.output.value.units()),
                script_pubkey: spend.utxo.output.script(),
            })
            .collect::<Vec<_>>();
        for (index, spend) in self.inputs.iter().enumerate() {
            let signing = InputSigner {
                transaction: &native,
                prevouts: &prevouts,
                sighash_type: self.sighash_type,
                network,
                signer: signers[index],
            };
            native.input[index].witness = signing.witness(index, spend).await?;
        }
        let id = TransactionId::from(native.compute_txid());
        SignedTransaction::from_envelope(id, Envelope::new(consensus::serialize(&native)))
    }
}

impl<S: Signer + ?Sized> InputSigner<'_, S> {
    async fn witness(&self, index: usize, spend: &Spend) -> Result<Witness, Error> {
        let script = &self.prevouts[index].script_pubkey;
        if script.is_p2wpkh() {
            self.sign_p2wpkh_input(index, spend).await
        } else if script.is_p2tr() {
            self.sign_p2tr_input(index).await
        } else {
            Err(Error::invalid_transaction(format!(
                "Bitcoin input {index} is neither P2WPKH nor P2TR"
            )))
        }
    }

    async fn sign_p2wpkh_input(&self, input_index: usize, input: &Spend) -> Result<Witness, Error> {
        let script = &self.prevouts[input_index].script_pubkey;
        let sighash_type = self.sighash_type.ecdsa()?;
        let sighash = SighashCache::new(self.transaction)
            .p2wpkh_signature_hash(
                input_index,
                script,
                Amount::from_sat(input.utxo.output.value.units()),
                sighash_type,
            )
            .map_err(|error| {
                Error::invalid_transaction(format!(
                    "could not compute Bitcoin input sighash: {error}"
                ))
            })?;
        let signed = self
            .signer
            .sign(SignRequest {
                payload: SignablePayload::Digest(Digest {
                    bytes: sighash.to_byte_array().to_vec(),
                }),
                scheme: SignatureScheme::EcdsaSecp256k1,
                encoding: SignatureEncoding::Der,
                public_key_format: PublicKeyFormat::Compressed,
                key_tweak: None,
            })
            .await
            .map_err(|error| Error::signing(format!("Bitcoin signing failed: {error}")))?;
        self.p2wpkh_witness(input_index, signed, sighash, sighash_type)
    }

    fn p2wpkh_witness(
        &self,
        input_index: usize,
        signed: SignedPayload,
        sighash: SegwitV0Sighash,
        sighash_type: EcdsaSighashType,
    ) -> Result<Witness, Error> {
        let script = &self.prevouts[input_index].script_pubkey;
        let public_key =
            CompressedPublicKey::from_slice(&signed.public_key.bytes).map_err(|error| {
                Error::signing(format!("invalid compressed Bitcoin public key: {error}"))
            })?;
        if NativeAddress::p2wpkh(&public_key, self.network.native()).script_pubkey() != *script {
            return Err(Error::signing(format!(
                "Bitcoin input {input_index} does not belong to its signing key"
            )));
        }
        let signature = signed.signature;
        if signature.scheme != SignatureScheme::EcdsaSecp256k1
            || signature.encoding != SignatureEncoding::Der
        {
            return Err(Error::signing(
                "Bitcoin signer returned an incompatible ECDSA signature",
            ));
        }
        let signature = ecdsa::Signature::from_der(&signature.bytes)
            .map_err(|error| Error::signing(format!("invalid DER Bitcoin signature: {error}")))?;
        let message = Message::from_digest(sighash.to_byte_array());
        Secp256k1::verification_only()
            .verify_ecdsa(&message, &signature, &public_key.0)
            .map_err(|_| {
                Error::signing(format!(
                    "Bitcoin signer returned an ECDSA signature that failed \
                     cryptographic verification for input {input_index}"
                ))
            })?;
        let signature = bitcoin::ecdsa::Signature {
            signature,
            sighash_type,
        };
        let signature = signature.serialize();
        let public_key = public_key.to_bytes();
        let signature_bytes: &[u8] = signature.as_ref();
        Ok(Witness::from_slice(&[
            signature_bytes.to_vec(),
            public_key.to_vec(),
        ]))
    }

    async fn sign_p2tr_input(&self, input_index: usize) -> Result<Witness, Error> {
        let sighash_type = self.sighash_type.taproot();
        let sighash = SighashCache::new(self.transaction)
            .taproot_key_spend_signature_hash(
                input_index,
                &Prevouts::All(self.prevouts),
                sighash_type,
            )
            .map_err(|error| {
                Error::invalid_transaction(format!(
                    "could not compute Taproot input sighash: {error}"
                ))
            })?;
        let signed = self
            .signer
            .sign(SignRequest {
                payload: SignablePayload::Digest(Digest {
                    bytes: sighash.to_byte_array().to_vec(),
                }),
                scheme: SignatureScheme::SchnorrSecp256k1,
                encoding: SignatureEncoding::Raw,
                public_key_format: PublicKeyFormat::XOnly,
                key_tweak: Some(ScalarTweak::TaggedHashAdd {
                    tag: b"TapTweak".to_vec(),
                    suffix: Vec::new(),
                }),
            })
            .await
            .map_err(|error| Error::signing(format!("Bitcoin signing failed: {error}")))?;
        self.p2tr_witness(input_index, signed, sighash, sighash_type)
    }

    fn p2tr_witness(
        &self,
        input_index: usize,
        signed: SignedPayload,
        sighash: TapSighash,
        sighash_type: TapSighashType,
    ) -> Result<Witness, Error> {
        let signature = signed.signature;
        let public_key = XOnlyPublicKey::from_slice(&signed.public_key.bytes).map_err(|error| {
            Error::signing(format!("invalid x-only Bitcoin public key: {error}"))
        })?;
        let secp = Secp256k1::verification_only();
        let expected =
            NativeAddress::p2tr(&secp, public_key, None, self.network.native()).script_pubkey();
        if expected != self.prevouts[input_index].script_pubkey {
            return Err(Error::signing(format!(
                "Bitcoin Taproot input {input_index} does not belong to its signing key"
            )));
        }
        if signature.scheme != SignatureScheme::SchnorrSecp256k1
            || signature.encoding != SignatureEncoding::Raw
        {
            return Err(Error::signing(
                "Bitcoin signer returned an incompatible Schnorr signature",
            ));
        }
        let signature = schnorr::Signature::from_slice(&signature.bytes).map_err(|error| {
            Error::signing(format!("invalid raw Bitcoin Schnorr signature: {error}"))
        })?;
        let (output_key, _) = public_key.tap_tweak(&secp, None);
        let message = Message::from_digest(sighash.to_byte_array());
        secp.verify_schnorr(&signature, &message, output_key.as_x_only_public_key())
            .map_err(|_| {
                Error::signing(format!(
                    "Bitcoin signer returned a Schnorr signature that failed \
                     cryptographic verification for input {input_index}"
                ))
            })?;
        let signature = bitcoin::taproot::Signature {
            signature,
            sighash_type,
        };
        Ok(Witness::from_slice(&[signature.to_vec()]))
    }
}

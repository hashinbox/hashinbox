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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use base::{SignFuture, SignerError, SignerErrorKind};
    use bitcoin::ScriptBuf;
    use futures_executor::block_on;

    use super::*;
    use crate::native::{
        ErrorKind, Satoshi,
        block::{Outpoint, Output, Utxo},
    };

    struct RecordingSigner {
        requests: Mutex<Vec<SignRequest>>,
        response: base::SignedPayload,
    }

    impl Signer for RecordingSigner {
        fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
            self.requests.lock().unwrap().push(request);
            Box::pin(async { Ok(self.response.clone()) })
        }
    }

    struct SigningCase {
        transaction: Transaction,
        prevouts: Vec<TxOut>,
        input: Spend,
        mode: SighashType,
        taproot: bool,
        request: SignRequest,
        response: base::SignedPayload,
        witness: Witness,
        wrong_signature: Vec<u8>,
        wrong_key: Vec<u8>,
    }

    impl SigningCase {
        fn new(taproot: bool, mode: SighashType) -> Self {
            let secp = Secp256k1::new();
            let secret = bitcoin::secp256k1::SecretKey::from_slice(&[1; 32]).unwrap();
            let other = bitcoin::secp256k1::SecretKey::from_slice(&[2; 32]).unwrap();
            let pair = bitcoin::secp256k1::Keypair::from_secret_key(&secp, &secret);
            let other_pair = bitcoin::secp256k1::Keypair::from_secret_key(&secp, &other);
            let public = CompressedPublicKey(pair.public_key());
            let internal = pair.x_only_public_key().0;
            let address = if taproot {
                NativeAddress::p2tr(&secp, internal, None, Network::Regtest.native())
            } else {
                NativeAddress::p2wpkh(&public, Network::Regtest.native())
            };
            let input = Spend {
                utxo: Utxo {
                    outpoint: Outpoint {
                        transaction_id: TransactionId([4; 32]),
                        output_index: 1,
                    },
                    output: Output {
                        value: Satoshi::from_atomic(20_000),
                        script_pubkey: address.script_pubkey().into_bytes(),
                        address: None,
                    },
                },
                sequence: 7,
            };
            let mut first = input.clone();
            first.utxo.outpoint.transaction_id = TransactionId([3; 32]);
            first.utxo.output.value = Satoshi::from_atomic(10_000);
            let unsigned = UnsignedTransaction {
                version: 2,
                lock_time: 11,
                inputs: vec![first, input.clone()],
                outputs: Vec::new(),
                sighash_type: mode,
            };
            let transaction = unsigned.native(Network::Regtest).unwrap();
            let prevouts = unsigned
                .inputs
                .iter()
                .map(|input| TxOut {
                    value: Amount::from_sat(input.utxo.output.value.units()),
                    script_pubkey: input.utxo.output.script(),
                })
                .collect::<Vec<_>>();
            let mut cache = SighashCache::new(&transaction);
            let digest = if taproot {
                cache
                    .taproot_key_spend_signature_hash(1, &Prevouts::All(&prevouts), mode.taproot())
                    .unwrap()
                    .to_byte_array()
            } else {
                cache
                    .p2wpkh_signature_hash(
                        1,
                        &prevouts[1].script_pubkey,
                        prevouts[1].value,
                        mode.ecdsa().unwrap(),
                    )
                    .unwrap()
                    .to_byte_array()
            };
            let message = Message::from_digest(digest);
            let wrong_message = Message::from_digest([9; 32]);
            let (bytes, wrong_signature, public_key, wrong_key, scheme, encoding, format, tweak) =
                if taproot {
                    let tweaked = pair.tap_tweak(&secp, None).to_keypair();
                    (
                        secp.sign_schnorr_no_aux_rand(&message, &tweaked)
                            .as_ref()
                            .to_vec(),
                        secp.sign_schnorr_no_aux_rand(&wrong_message, &tweaked)
                            .as_ref()
                            .to_vec(),
                        internal.serialize().to_vec(),
                        other_pair.x_only_public_key().0.serialize().to_vec(),
                        SignatureScheme::SchnorrSecp256k1,
                        SignatureEncoding::Raw,
                        PublicKeyFormat::XOnly,
                        Some(ScalarTweak::TaggedHashAdd {
                            tag: b"TapTweak".to_vec(),
                            suffix: Vec::new(),
                        }),
                    )
                } else {
                    (
                        secp.sign_ecdsa(&message, &secret).serialize_der().to_vec(),
                        secp.sign_ecdsa(&wrong_message, &secret)
                            .serialize_der()
                            .to_vec(),
                        public.to_bytes().to_vec(),
                        other_pair.public_key().serialize().to_vec(),
                        SignatureScheme::EcdsaSecp256k1,
                        SignatureEncoding::Der,
                        PublicKeyFormat::Compressed,
                        None,
                    )
                };
            let mut witness_signature = bytes.clone();
            if !taproot || mode != SighashType::TaprootDefault {
                witness_signature.push(mode.taproot() as u8);
            }
            let witness = if taproot {
                Witness::from_slice(&[witness_signature])
            } else {
                Witness::from_slice(&[witness_signature, public_key.clone()])
            };
            Self {
                transaction,
                prevouts,
                input,
                mode,
                taproot,
                witness,
                wrong_signature,
                wrong_key,
                request: SignRequest {
                    payload: SignablePayload::Digest(Digest {
                        bytes: digest.to_vec(),
                    }),
                    scheme,
                    encoding,
                    public_key_format: format,
                    key_tweak: tweak,
                },
                response: base::SignedPayload {
                    signature: base::Signature {
                        scheme,
                        encoding,
                        bytes,
                    },
                    public_key: base::PublicKey {
                        curve: base::Curve::Secp256k1,
                        format,
                        bytes: public_key,
                    },
                },
            }
        }

        fn sign(&self, signer: &dyn Signer) -> Result<Witness, Error> {
            let signing = InputSigner {
                transaction: &self.transaction,
                prevouts: &self.prevouts,
                sighash_type: self.mode,
                network: Network::Regtest,
                signer,
            };
            if self.taproot {
                block_on(signing.sign_p2tr_input(1))
            } else {
                block_on(signing.sign_p2wpkh_input(1, &self.input))
            }
        }
    }

    #[test]
    fn input_signing_preserves_exact_requests_and_witness_sighash_bytes() {
        for (taproot, mode) in [
            (false, SighashType::All),
            (false, SighashType::NoneAnyoneCanPay),
            (true, SighashType::TaprootDefault),
            (true, SighashType::All),
            (true, SighashType::NoneAnyoneCanPay),
        ] {
            let case = SigningCase::new(taproot, mode);
            let signer = RecordingSigner {
                requests: Mutex::new(Vec::new()),
                response: case.response.clone(),
            };
            assert_eq!(case.sign(&signer).unwrap(), case.witness);
            assert_eq!(*signer.requests.lock().unwrap(), vec![case.request]);
        }
    }

    #[test]
    fn invalid_input_sighashes_fail_before_calling_the_signer() {
        for (taproot, mode, message) in [
            (
                false,
                SighashType::TaprootDefault,
                "Taproot default sighash cannot sign a P2WPKH input",
            ),
            (
                true,
                SighashType::Single,
                "could not compute Taproot input sighash:",
            ),
        ] {
            let mut case = SigningCase::new(taproot, SighashType::All);
            case.mode = mode;
            let signer = RecordingSigner {
                requests: Mutex::new(Vec::new()),
                response: case.response.clone(),
            };
            let error = case.sign(&signer).unwrap_err();
            assert_eq!(error.kind, ErrorKind::InvalidTransaction);
            assert!(error.message.starts_with(message), "{}", error.message);
            assert!(signer.requests.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn input_signing_preserves_key_signature_and_verification_error_order() {
        for taproot in [false, true] {
            let case = SigningCase::new(taproot, SighashType::All);
            let mut malformed_key = case.response.clone();
            malformed_key.public_key.bytes.clear();
            malformed_key.signature.scheme = SignatureScheme::Ed25519;
            let mut wrong_key = malformed_key.clone();
            wrong_key.public_key.bytes = case.wrong_key.clone();
            let mut wrong_scheme = malformed_key.clone();
            wrong_scheme.public_key = case.response.public_key.clone();
            wrong_scheme.signature.bytes.clear();
            let mut wrong_encoding = case.response.clone();
            wrong_encoding.signature.encoding = SignatureEncoding::Compact;
            wrong_encoding.signature.bytes.clear();
            let mut malformed_signature = case.response.clone();
            malformed_signature.signature.bytes.clear();
            let mut unverified = case.response.clone();
            unverified.signature.bytes = case.wrong_signature.clone();
            let (key_error, ownership, incompatible, malformed, verification) = if taproot {
                (
                    "invalid x-only Bitcoin public key:",
                    "Bitcoin Taproot input 1 does not belong to its signing key",
                    "Bitcoin signer returned an incompatible Schnorr signature",
                    "invalid raw Bitcoin Schnorr signature:",
                    "Bitcoin signer returned a Schnorr signature that failed \
                     cryptographic verification for input 1",
                )
            } else {
                (
                    "invalid compressed Bitcoin public key:",
                    "Bitcoin input 1 does not belong to its signing key",
                    "Bitcoin signer returned an incompatible ECDSA signature",
                    "invalid DER Bitcoin signature:",
                    "Bitcoin signer returned an ECDSA signature that failed \
                     cryptographic verification for input 1",
                )
            };
            for (response, message) in [
                (malformed_key, key_error),
                (wrong_key, ownership),
                (wrong_scheme, incompatible),
                (wrong_encoding, incompatible),
                (malformed_signature, malformed),
                (unverified, verification),
            ] {
                let signer = RecordingSigner {
                    requests: Mutex::new(Vec::new()),
                    response,
                };
                let error = case.sign(&signer).unwrap_err();
                assert_eq!(error.kind, ErrorKind::Signer);
                if message.ends_with(':') {
                    assert!(error.message.starts_with(message), "{}", error.message);
                } else {
                    assert_eq!(error.message, message);
                }
                assert_eq!(*signer.requests.lock().unwrap(), vec![case.request.clone()]);
            }
        }
    }

    struct FailingSigner {
        requests: Mutex<Vec<SignRequest>>,
        error: SignerError,
    }

    impl Signer for FailingSigner {
        fn sign<'a>(&'a self, request: SignRequest) -> SignFuture<'a> {
            self.requests.lock().unwrap().push(request);
            Box::pin(async { Err(self.error.clone()) })
        }
    }

    /// One input spending `script`, no outputs.
    fn spending(script: ScriptBuf) -> UnsignedTransaction {
        let utxo = Utxo {
            outpoint: Outpoint {
                transaction_id: TransactionId([3; 32]),
                output_index: 0,
            },
            output: Output {
                value: Satoshi::from_atomic(10_000),
                script_pubkey: script.into_bytes(),
                address: None,
            },
        };
        UnsignedTransaction {
            version: 2,
            lock_time: 0,
            inputs: vec![Spend { utxo, sequence: 0 }],
            outputs: Vec::new(),
            sighash_type: SighashType::All,
        }
    }

    #[test]
    fn both_input_kinds_preserve_signer_failure_context_and_signing_request() {
        for (script, scheme, encoding, public_key_format, key_tweak) in [
            (
                ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::from_byte_array([7; 20])),
                SignatureScheme::EcdsaSecp256k1,
                SignatureEncoding::Der,
                PublicKeyFormat::Compressed,
                None,
            ),
            (
                ScriptBuf::from_bytes([vec![0x51, 0x20], vec![7; 32]].concat()),
                SignatureScheme::SchnorrSecp256k1,
                SignatureEncoding::Raw,
                PublicKeyFormat::XOnly,
                Some(ScalarTweak::TaggedHashAdd {
                    tag: b"TapTweak".to_vec(),
                    suffix: Vec::new(),
                }),
            ),
        ] {
            let signer = FailingSigner {
                requests: Mutex::new(Vec::new()),
                error: SignerError {
                    kind: SignerErrorKind::UnsupportedOperation,
                    message: " signer unavailable\nrequest context ".to_owned(),
                },
            };
            let unsigned = spending(script);
            let error = block_on(unsigned.sign(Network::Regtest, &signer)).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Signer);
            assert_eq!(
                error.message,
                "Bitcoin signing failed:  signer unavailable\nrequest context "
            );
            let requests = signer.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].scheme, scheme);
            assert_eq!(requests[0].encoding, encoding);
            assert_eq!(requests[0].public_key_format, public_key_format);
            assert_eq!(requests[0].key_tweak, key_tweak);
            assert!(matches!(
                &requests[0].payload,
                SignablePayload::Digest(digest) if digest.bytes.len() == 32
            ));
        }
    }
}

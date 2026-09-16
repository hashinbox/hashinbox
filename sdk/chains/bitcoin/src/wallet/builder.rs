use base::{
    Address as BaseAddress, Decimal, TransactionBuilder as BaseBuilder, TransactionError,
    TransactionErrorKind, TransactionFuture, TransactionId as BaseTransactionId,
    TransactionSnapshot,
};

use super::provider::{PREPARED_KIND, SNAPSHOT_KIND, Wallet};
use crate::{Address, BuildRequest, Output, SpendSource, TransactionBuilder, TransactionId};

pub(super) struct Builder {
    scope: indexing::IndexScope,
    network: crate::Network,
    source: Address,
    signer: std::sync::Arc<crate::KeyPair>,
    utxos: std::sync::Arc<crate::IndexUtxos>,
    fees: std::sync::Arc<dyn crate::Fees>,
    fee_target_blocks: u16,
    max_fee_rate: crate::FeeRate,
    recipients: Vec<(Address, Decimal)>,
}

impl Wallet {
    pub(super) fn builder(&self) -> Builder {
        Builder {
            scope: self.config.scope.clone(),
            network: self.config.network,
            source: self.key.pair.address.clone(),
            signer: self.key.clone(),
            utxos: self.utxos.clone(),
            fees: self.fees.clone(),
            fee_target_blocks: self.config.fee_target_blocks,
            max_fee_rate: self.config.max_fee_rate,
            recipients: Vec::new(),
        }
    }
}

impl Builder {
    pub(super) fn restore(
        wallet: &Wallet,
        snapshot: &TransactionSnapshot,
    ) -> Result<Self, TransactionError> {
        let mut builder = wallet.builder();
        builder.recipients =
            super::snapshot::decode(&wallet.config, &wallet.key.pair.address, snapshot)?;
        builder.validate()?;
        Ok(builder)
    }

    async fn funding(&self) -> Result<Vec<SpendSource>, TransactionError> {
        self.validate()?;
        if self.recipients.is_empty() {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
            ));
        }
        let set = self
            .utxos
            .utxos(vec![self.source.clone()])
            .await
            .map_err(|error| {
                TransactionError::new(TransactionErrorKind::Unavailable, error.to_string())
            })?;
        set.outputs
            .into_iter()
            .map(|output| {
                SpendSource::from_exact_selection(
                    self.network,
                    &self.source,
                    TransactionId(output.transaction_id),
                    output.output_index,
                    output.value,
                    output.script_pubkey,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| {
                TransactionError::new(TransactionErrorKind::InvalidTransaction, error.to_string())
            })
    }

    fn validate(&self) -> Result<(), TransactionError> {
        if self.scope.chain.0 != "bitcoin" || self.scope.network != self.network.canonical_name() {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidSnapshot,
                "Bitcoin transaction identity, chain, and network do not agree",
            ));
        }
        Address::parse_for_network(self.source.encoded(), self.network).map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidSnapshot, error.to_string())
        })?;
        Ok(())
    }
}

impl BaseBuilder for Builder {
    fn transfer(
        &mut self,
        destination: BaseAddress,
        amount: Decimal,
    ) -> Result<(), TransactionError> {
        let value = std::str::from_utf8(destination.as_bytes()).map_err(|_| {
            TransactionError::new(
                TransactionErrorKind::InvalidAddress,
                "Bitcoin address is not UTF-8",
            )
        })?;
        let address = Address::parse_for_network(value, self.network).map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidAddress, error.to_string())
        })?;
        crate::Satoshi::from_decimal(&amount).map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidAmount, error.to_string())
        })?;
        self.recipients.push((address, amount));
        Ok(())
    }

    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        self.validate()?;
        if self.recipients.is_empty() {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
            ));
        }
        let transfers = self
            .recipients
            .iter()
            .map(|(destination, amount)| {
                serde_json::json!({
                    "destination": destination.to_string(),
                    "amount": amount.to_string(),
                })
            })
            .collect::<Vec<_>>();
        Ok(TransactionSnapshot::new(
            SNAPSHOT_KIND,
            serde_json::json!({
                "scope": {
                    "chain": self.scope.chain.0.as_str(),
                    "network": self.scope.network.as_str(),
                },
                "source": self.source.to_string(),
                "asset": {
                    "kind": "native",
                    "ticker": crate::BTC.ticker,
                    "decimals": crate::BTC.decimals,
                },
                "transfers": transfers,
                "change": self.source.to_string(),
            }),
        ))
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            let available = self.funding().await?;
            let recipients = self
                .recipients
                .iter()
                .cloned()
                .map(|(address, amount)| Output::new(address, amount))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| {
                    TransactionError::new(TransactionErrorKind::InvalidAmount, error.to_string())
                })?;
            let fee_rate = self
                .fees
                .estimate(self.fee_target_blocks)
                .await
                .map_err(|error| {
                    TransactionError::new(TransactionErrorKind::Unavailable, error.to_string())
                })?;
            if fee_rate > self.max_fee_rate {
                return Err(TransactionError::new(
                    TransactionErrorKind::Fee,
                    "estimated Bitcoin fee rate exceeds the configured maximum",
                ));
            }
            let request = BuildRequest {
                available,
                recipients,
                change_address: self.source.clone(),
                fee_rate,
                drain_wallet: false,
            };
            let signed = TransactionBuilder::new(self.network, request)
                .sign(self.signer.as_ref())
                .await
                .map_err(|error| {
                    TransactionError::new(TransactionErrorKind::Signing, error.to_string())
                })?;
            Ok(base::SignedTransaction::new(
                PREPARED_KIND,
                BaseTransactionId::new(signed.id().to_string()),
                base::TransactionEnvelope::new(signed.consensus_bytes().to_vec()),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{consensus, hashes::Hash};
    use futures_executor::block_on;
    use indexing::{BoxFuture, IndexError, OutputPage, OutputRequest, Outputs, SourceError};
    use std::sync::{Arc, Mutex};

    struct Readbacks {
        events: Mutex<Vec<&'static str>>,
        output_error: Option<IndexError>,
        fee: Result<crate::FeeRate, SourceError>,
    }

    impl Outputs for Readbacks {
        fn list<'a>(
            &'a self,
            request: OutputRequest,
        ) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
            Box::pin(async move {
                self.events.lock().unwrap().push("outputs");
                if let Some(error) = &self.output_error {
                    return Err(error.clone());
                }
                let script =
                    Address::parse_for_network(&request.address.value, crate::Network::Regtest)
                        .unwrap()
                        .script_pubkey_for_network(crate::Network::Regtest)
                        .unwrap()
                        .into_bytes();
                let transaction = indexing::TransactionRef {
                    scope: request.scope,
                    value: TransactionId([3; 32]).to_string(),
                };
                let output = indexing::IndexedOutput {
                    id: indexing::OutputId {
                        transaction,
                        index: 2,
                    },
                    address: request.address,
                    asset: indexing::AssetId {
                        chain: indexing::ChainId(crate::CHAIN.to_owned()),
                        asset: "native".to_owned(),
                    },
                    amount: Decimal::from(100_000_u64),
                    evidence: script,
                    created_at: indexing::BlockHeight(1),
                    coinbase: false,
                };
                Ok(OutputPage {
                    checkpoint: Some(indexing::BlockRef {
                        position: indexing::BlockPosition(1),
                        height: indexing::BlockHeight(1),
                        hash: indexing::BlockHash(vec![1; 32]),
                        parent: None,
                        timestamp: None,
                    }),
                    outputs: vec![output],
                    next: None,
                })
            })
        }
    }

    impl crate::Fees for Readbacks {
        fn estimate<'a>(
            &'a self,
            target: u16,
        ) -> BoxFuture<'a, Result<crate::FeeRate, SourceError>> {
            Box::pin(async move {
                assert_eq!(target, 6);
                self.events.lock().unwrap().push("fees");
                self.fee.clone()
            })
        }
    }

    impl Builder {
        fn fixture(readbacks: Arc<Readbacks>) -> Self {
            let network = crate::Network::Regtest;
            let scope = indexing::IndexScope {
                chain: indexing::ChainId(crate::CHAIN.to_owned()),
                network: network.canonical_name().to_owned(),
            };
            let key = crypto::SecretKey::new(vec![1; 32]).unwrap();
            let public = key.public_key(crypto::PublicKeyFormat::Compressed).unwrap();
            let public = bitcoin::PublicKey::from_slice(&public.bytes).unwrap();
            let compressed = bitcoin::CompressedPublicKey::try_from(public).unwrap();
            let source = Address::from_encoded(
                bitcoin::Address::p2wpkh(&compressed, network.native()).to_string(),
            );
            Self {
                signer: Arc::new(
                    crate::KeyPair::import(
                        wallets::SecretBytes::new([1; 32]),
                        network,
                        crate::AddressType::SegwitV0,
                    )
                    .unwrap(),
                ),
                utxos: Arc::new(
                    crate::IndexUtxos::new(scope.clone(), network, readbacks.clone()).unwrap(),
                ),
                scope,
                network,
                recipients: vec![(source.clone(), "0.00001".parse().unwrap())],
                source,
                fees: readbacks,
                fee_target_blocks: 6,
                max_fee_rate: crate::FeeRate::new(1_000),
            }
        }
    }

    #[test]
    fn preparation_preserves_validation_funding_amount_and_fee_order() {
        use TransactionErrorKind::{
            Fee, InvalidAmount, InvalidSnapshot, InvalidTransaction, Unavailable,
        };
        let messages = [
            "Bitcoin transaction identity, chain, and network do not agree",
            "transaction has no recipients",
            "output failure",
            "Bitcoin selected output must be P2WPKH or P2TR",
            "amount has more than 8 fractional digits",
            "fee failure",
            "estimated Bitcoin fee rate exceeds the configured maximum",
        ];
        for (stage, kind, events) in [
            (0, InvalidSnapshot, &[][..]),
            (1, InvalidTransaction, &[][..]),
            (2, Unavailable, &["outputs"][..]),
            (3, InvalidTransaction, &["outputs"][..]),
            (4, InvalidAmount, &["outputs"][..]),
            (5, Unavailable, &["outputs", "fees"][..]),
            (6, Fee, &["outputs", "fees"][..]),
        ] {
            let readbacks = Arc::new(Readbacks {
                events: Mutex::new(Vec::new()),
                output_error: (stage == 2).then(|| {
                    IndexError::new(indexing::IndexErrorKind::Store, "output failure", true)
                }),
                fee: if stage == 5 {
                    Err(SourceError {
                        message: "fee failure".to_owned(),
                        retryable: true,
                    })
                } else {
                    Ok(crate::FeeRate::new(1_001))
                },
            });
            let mut builder = Builder::fixture(readbacks.clone());
            match stage {
                0 => builder.scope.network = "mainnet".to_owned(),
                1 => builder.recipients.clear(),
                3 => builder.source = Address::from_encoded("mipcBbFg9gMiCh81Kj8tqqdgoZub1ZJRfn"),
                4 => builder.recipients[0].1 = "0.000000001".parse().unwrap(),
                _ => {}
            }
            let error = block_on(builder.prepare()).unwrap_err();
            assert_eq!(error.kind, kind);
            assert_eq!(error.message, messages[stage]);
            assert_eq!(error.ambiguous_transaction_id, None);
            assert_eq!(readbacks.events.lock().unwrap().as_slice(), events);
        }
    }

    #[test]
    fn preparation_keeps_signed_envelope_identity_and_atomic_outputs() {
        let readbacks = Arc::new(Readbacks {
            events: Mutex::new(Vec::new()),
            output_error: None,
            fee: Ok(crate::FeeRate::new(1_000)),
        });
        let mut builder = Builder::fixture(readbacks.clone());
        let prepared = block_on(builder.prepare()).unwrap();
        assert_eq!(
            readbacks.events.lock().unwrap().as_slice(),
            ["outputs", "fees"]
        );
        assert_eq!(prepared.kind(), PREPARED_KIND);
        let native: bitcoin::Transaction =
            consensus::deserialize(prepared.envelope().as_bytes()).unwrap();
        assert_eq!(prepared.id().as_str(), native.compute_txid().to_string());
        assert_eq!(native.input.len(), 1);
        assert_eq!(
            native.input[0].previous_output,
            bitcoin::OutPoint::new(bitcoin::Txid::from_byte_array([3; 32]), 2)
        );
        assert_eq!(native.input[0].witness.len(), 2);
        let script = builder
            .source
            .script_pubkey_for_network(builder.network)
            .unwrap();
        assert_eq!(
            native.output,
            [1_000, 98_859].map(|value| bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(value),
                script_pubkey: script.clone(),
            })
        );
    }
}

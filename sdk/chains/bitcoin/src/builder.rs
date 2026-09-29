//! [`TransferBuilder`]: the `base::TransactionBuilder` of a Bitcoin wallet,
//! with its snapshot encoding.

use base::{
    Decimal, Draft, TransactionBuilder, TransactionError, TransactionErrorKind, TransactionFuture,
    TransactionSnapshot, Transfer,
};
use serde::Deserialize;

use crate::native::{
    Address, AssetKind, BTC, CHAIN_NAME, Satoshi, SignedTransaction, block::Outpoint,
};
use crate::sender::Item;
use crate::wallet::Wallet;

/// Builds one transaction from one Bitcoin wallet: add transfers, snapshot
/// them, prepare (estimate the fee, select and reserve outputs, sign) them
/// through the wallet's outbox.
pub struct TransferBuilder {
    wallet: Wallet,
    transfers: Vec<(Address, Decimal)>,
}

impl TransferBuilder {
    /// Snapshot kind of an unsigned transfer.
    pub const SNAPSHOT_KIND: &str = "bitcoin.transfer";
    /// Kind of a prepared (signed) transaction.
    pub const PREPARED_KIND: &str = "bitcoin.signed";

    pub(crate) fn new(wallet: &Wallet) -> Self {
        Self {
            wallet: wallet.clone(),
            transfers: Vec::new(),
        }
    }

    /// Rebuilds a builder from a snapshot this wallet produced.
    pub(crate) fn restore(
        wallet: &Wallet,
        snapshot: &TransactionSnapshot,
    ) -> Result<Self, TransactionError> {
        let mut builder = Self::new(wallet);
        builder.transfers = builder.decode_snapshot(snapshot)?;
        builder.validate()?;
        Ok(builder)
    }

    /// The native signed transaction inside a prepared SDK transaction.
    pub(crate) fn decode(
        prepared: &base::SignedTransaction,
    ) -> Result<SignedTransaction, TransactionError> {
        if prepared.version() != base::SignedTransaction::VERSION
            || prepared.kind() != Self::PREPARED_KIND
        {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "prepared transaction is not a Bitcoin signed envelope",
            ));
        }
        let id = prepared.id().as_str().parse()?;
        Ok(SignedTransaction::from_envelope(
            id,
            prepared.envelope().clone(),
        )?)
    }

    fn from(&self) -> &Address {
        &self.wallet.address
    }

    fn validate(&self) -> Result<(), TransactionError> {
        self.wallet
            .config
            .validate()
            .map_err(|error| invalid(error.message))
    }

    /// One outbox item per configured transfer, all from this wallet.
    fn configured(&self) -> Result<Vec<Item>, TransactionError> {
        if self.transfers.is_empty() {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
            ));
        }
        self.transfers
            .iter()
            .map(|(to, amount)| {
                self.wallet.item(to.clone(), amount).map_err(|error| {
                    TransactionError::new(TransactionErrorKind::InvalidAmount, error.message)
                })
            })
            .collect()
    }

    /// The chain-owned durable encoding of this builder, inside its
    /// [`Draft`].
    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        self.validate()?;
        self.configured()?;
        let transfers = self
            .transfers
            .iter()
            .map(|(destination, amount)| {
                serde_json::json!({
                    "destination": destination.as_str(),
                    "amount": amount.to_string(),
                })
            })
            .collect::<Vec<_>>();
        Ok(TransactionSnapshot::new(
            Self::SNAPSHOT_KIND,
            serde_json::json!({
                "scope": {
                    "chain": self.wallet.config.network.chain.as_str(),
                    "network": self.wallet.config.network.name.as_str(),
                },
                "source": self.from().as_str(),
                "asset": {"kind": "native", "ticker": BTC.ticker, "decimals": BTC.decimals},
                "transfers": transfers,
                "change": self.from().as_str(),
            }),
        ))
    }

    fn decode_snapshot(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<Vec<(Address, Decimal)>, TransactionError> {
        if snapshot.version() != TransactionSnapshot::VERSION
            || snapshot.kind() != Self::SNAPSHOT_KIND
        {
            return Err(invalid("snapshot is not a supported Bitcoin transfer"));
        }
        let body: SnapshotBody = serde_json::from_value(snapshot.value().clone())
            .map_err(|error| invalid(format!("invalid Bitcoin snapshot: {error}")))?;
        let source = self.from().as_str();
        if body.network.chain != CHAIN_NAME.as_str()
            || body.network.network != self.wallet.config.network.name
            || body.source != source
            || body.change != source
            || !body.asset.is_btc()
        {
            return Err(invalid(
                "Bitcoin snapshot does not belong to this wallet, network, or asset",
            ));
        }
        if body.transfers.is_empty() {
            return Err(invalid("Bitcoin snapshot has no transfers"));
        }
        body.transfers
            .into_iter()
            .map(|transfer| {
                let to = Address::parse(&transfer.destination, self.wallet.config.native)
                    .map_err(|error| invalid(error.message))?;
                let amount = transfer.amount.parse::<Decimal>().map_err(invalid)?;
                Satoshi::from_decimal(&amount).map_err(invalid)?;
                Ok((to, amount))
            })
            .collect()
    }
}

impl TransactionBuilder for TransferBuilder {
    fn transfer(
        &mut self,
        destination: base::Address,
        amount: Decimal,
    ) -> Result<(), TransactionError> {
        let to = self.wallet.config.destination(&destination)?;
        Satoshi::from_decimal(&amount).map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidAmount, error.to_string())
        })?;
        self.transfers.push((to, amount));
        Ok(())
    }

    fn draft(&self) -> Result<Draft, TransactionError> {
        Ok(Draft {
            source: self.from().canonical(&self.wallet.config.network),
            transfers: self
                .transfers
                .iter()
                .map(|(to, amount)| Transfer {
                    to: to.canonical(&self.wallet.config.network),
                    amount: amount.clone(),
                })
                .collect(),
            asset: AssetKind::Native.id(),
            snapshot: self.snapshot()?,
        })
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            self.validate()?;
            let items = self.configured()?;
            let (signed, fee, spends) = self.wallet.outbox.sign(items).await?;
            Ok(base::SignedTransaction::new(
                Self::PREPARED_KIND,
                signed.id.into(),
                signed.envelope,
                fee.atomic(),
            )
            .with_claim(claimed(spends)))
        })
    }
}

/// What the outbox held to sign: the outputs `spends` names, each under the
/// `txid:vout` spelling Bitcoin already uses for an outpoint.
fn claimed(spends: Vec<Outpoint>) -> base::Claim {
    base::Claim::new(
        spends
            .into_iter()
            .map(|outpoint| format!("{}:{}", outpoint.transaction_id, outpoint.output_index)),
    )
}

/// [`TransactionErrorKind::InvalidSnapshot`] with `error`'s text.
fn invalid(error: impl std::fmt::Display) -> TransactionError {
    TransactionError::new(TransactionErrorKind::InvalidSnapshot, error.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotBody {
    /// The persisted key is `scope`; the field is `network`, because that noun
    /// was retired and snapshots written by earlier builds must keep
    /// deserializing.
    #[serde(rename = "scope")]
    network: NetworkBody,
    source: String,
    asset: Asset,
    transfers: Vec<SnapshotTransfer>,
    change: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkBody {
    chain: String,
    network: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Asset {
    kind: String,
    ticker: String,
    decimals: u32,
}

impl Asset {
    fn is_btc(&self) -> bool {
        self.kind == "native" && self.ticker == BTC.ticker && self.decimals == BTC.decimals
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotTransfer {
    destination: String,
    amount: String,
}

#[cfg(test)]
mod tests {
    use base::{Networked, TransactionBuilder as _};
    use bitcoin::{consensus, hashes::Hash};
    use futures_executor::block_on;
    use indexing::{IndexError, IndexErrorKind};
    use wallets::TransactionFactory;

    use super::*;
    use crate::native::{AddressType, Error, Network, TransactionId, transaction::FeeRate};
    use std::sync::Arc;

    use crate::sender::stub::Node;
    use crate::wallet::WalletConfig;

    fn wallet(node: Node) -> (Arc<Node>, Wallet) {
        let key = Node::key(1);
        let address = key.address(Network::Regtest, AddressType::SegwitV0);
        let node = Arc::new(Node {
            outputs: node.outputs.clone().map(|outputs| {
                if outputs.is_empty() {
                    vec![(address.clone(), 100_000)]
                } else {
                    outputs
                }
            }),
            ..node
        });
        let config = WalletConfig::new(&Network::Regtest);
        let wallet = Wallet::fixture(config, key, address, &node);
        (node, wallet)
    }

    fn own(builder: &TransferBuilder) -> base::Address {
        builder.from().canonical(&Network::Regtest.canonical())
    }

    /// One prepared transfer of 0.00001 BTC from `wallet` to `to`.
    fn prepared(wallet: &Wallet, to: &base::Address) -> base::SignedTransaction {
        let mut builder = TransferBuilder::new(wallet);
        builder
            .transfer(to.clone(), "0.00001".parse().unwrap())
            .unwrap();
        block_on(builder.prepare()).expect("the transfer must sign")
    }

    #[test]
    fn a_signed_transaction_claims_the_outputs_its_outbox_selected() {
        let (alice, bob) = (Node::address(1), Node::address(2));
        let node = Arc::new(Node {
            outputs: Ok(vec![(alice.clone(), 100_000), (bob.clone(), 100_000)]),
            ..Node::default()
        });
        let config = WalletConfig::new(&Network::Regtest);
        let mine = Wallet::fixture(config.clone(), Node::key(1), alice, &node);
        let theirs = Wallet::fixture(config, Node::key(2), bob.clone(), &node);
        let destination = bob.canonical(&Network::Regtest.canonical());

        let first = prepared(&mine, &destination);
        let again = prepared(&mine, &destination);
        let other = prepared(&theirs, &destination);

        assert_eq!(
            first.claim(),
            Some(&base::Claim::new([format!("{}:0", TransactionId([1; 32]))])),
            "it names the exact output the outbox selected"
        );
        let (first, again, other) = (
            first.claim().unwrap(),
            again.claim().unwrap(),
            other.claim().unwrap(),
        );
        assert!(
            first.collides_with(again),
            "two in-flight sends of one wallet take the same output"
        );
        assert!(!first.collides_with(other), "another wallet's does not");
    }

    #[test]
    fn preparation_keeps_validation_amount_fee_and_funding_order() {
        let fee_failure = Error::rpc("fee failure", true);
        for (stage, kind, message, events) in [
            (
                0,
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
                0,
            ),
            (
                1,
                TransactionErrorKind::InvalidAmount,
                "amount has more than 8 fractional digits",
                0,
            ),
            (2, TransactionErrorKind::Unavailable, "output failure", 2),
            (3, TransactionErrorKind::Unavailable, "fee failure", 1),
            (
                4,
                TransactionErrorKind::Fee,
                "estimated Bitcoin fee rate exceeds the configured maximum",
                1,
            ),
            (
                5,
                TransactionErrorKind::InsufficientFunds,
                "insufficient Bitcoin funds for 99999999 satoshis plus network fee",
                2,
            ),
        ] {
            let node = Node {
                outputs: if stage == 2 {
                    Err(IndexError::new(
                        IndexErrorKind::Store,
                        "output failure",
                        true,
                    ))
                } else {
                    Ok(Vec::new())
                },
                fee: match stage {
                    3 => Err(fee_failure.clone()),
                    4 => Ok(FeeRate::from_sat_per_vb(10_001)),
                    _ => Ok(FeeRate::from_sat_per_kvb(1_000)),
                },
                ..Node::default()
            };
            let (node, wallet) = wallet(node);
            let mut builder = TransferBuilder::new(&wallet);
            let amount = match stage {
                1 => "0.000000001",
                5 => "0.99999999",
                _ => "0.00001",
            };
            if stage != 0 {
                let destination = own(&builder);
                builder
                    .transfer(destination, "0.00001".parse().unwrap())
                    .unwrap();
                builder.transfers[0].1 = amount.parse().unwrap();
            }
            let error = block_on(builder.prepare()).unwrap_err();
            assert_eq!(
                (error.kind, error.message.as_str()),
                (kind, message),
                "stage {stage}"
            );
            assert_eq!(error.ambiguous_transaction_id, None);
            assert_eq!(node.events().len(), events, "stage {stage}");
        }
    }

    #[test]
    fn preparation_signs_the_exact_envelope_with_change() {
        let (node, wallet) = wallet(Node::default());
        let mut builder = TransferBuilder::new(&wallet);
        let destination = own(&builder);
        builder
            .transfer(destination, "0.00001".parse().unwrap())
            .unwrap();
        let prepared = block_on(builder.prepare()).unwrap();
        assert_eq!(node.events()[0], "fees");
        assert_eq!(prepared.kind(), TransferBuilder::PREPARED_KIND);
        let native: bitcoin::Transaction =
            consensus::deserialize(prepared.envelope().as_bytes()).unwrap();
        assert_eq!(prepared.id().as_str(), native.compute_txid().to_string());
        assert_eq!(
            native.input[0].previous_output.txid,
            bitcoin::Txid::from_byte_array([1; 32])
        );
        assert_eq!(native.input[0].witness.len(), 2);
        let values: Vec<_> = native
            .output
            .iter()
            .map(|output| output.value.to_sat())
            .collect();
        assert_eq!(values, [1_000, 98_859]);
    }

    #[test]
    fn snapshots_round_trip_and_bind_wallet_network_and_asset() {
        let (_, wallet) = wallet(Node::default());
        let mut builder = TransferBuilder::new(&wallet);
        let destination = own(&builder);
        builder
            .transfer(destination, "1.25".parse().unwrap())
            .unwrap();
        let draft = builder.draft().unwrap();
        let snapshot = draft.snapshot.clone();
        assert_eq!(draft.source, own(&builder));
        assert_eq!(draft.asset, AssetKind::Native.id());
        assert_eq!(
            draft
                .transfers
                .iter()
                .map(|transfer| (transfer.to.value.clone(), transfer.amount.to_string()))
                .collect::<Vec<_>>(),
            vec![(own(&builder).value, "1.25".to_owned())]
        );
        assert_eq!(
            snapshot.value()["asset"],
            serde_json::json!({"kind": "native", "ticker": "BTC", "decimals": 8})
        );
        let restored = wallet.resume(&snapshot).unwrap();
        assert_eq!(restored.draft().unwrap().snapshot, snapshot);
        let mut other = snapshot.value().clone();
        for (field, value) in [
            ("/scope/network", serde_json::json!("mainnet")),
            ("/scope/chain", serde_json::json!("ethereum")),
            (
                "/source",
                serde_json::json!("1BoatSLRHtKNngkdXEeobR76b53LETtpyT"),
            ),
            ("/asset/decimals", serde_json::json!(18)),
        ] {
            let original = other.pointer(field).unwrap().clone();
            *other.pointer_mut(field).unwrap() = value;
            let error = wallet
                .resume(&TransactionSnapshot::new(
                    TransferBuilder::SNAPSHOT_KIND,
                    other.clone(),
                ))
                .err()
                .unwrap();
            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot);
            *other.pointer_mut(field).unwrap() = original;
        }
        let own = wallet.address.as_str();
        for (transfers, message) in [
            (serde_json::json!([]), "Bitcoin snapshot has no transfers"),
            (
                serde_json::json!([{"destination": own, "amount": "-1"}]),
                "currency amount must not be negative",
            ),
            (
                serde_json::json!([{"destination": own, "amount": "0.000000001"}]),
                "amount has more than 8 fractional digits",
            ),
        ] {
            other["transfers"] = transfers;
            let snapshot = TransactionSnapshot::new(TransferBuilder::SNAPSHOT_KIND, other.clone());
            let error = wallet.resume(&snapshot).err().unwrap();
            assert_eq!(
                (error.kind, error.message.as_str()),
                (TransactionErrorKind::InvalidSnapshot, message)
            );
        }
        let foreign = TransactionSnapshot::new("ethereum.transfer", serde_json::json!({}));
        assert_eq!(
            wallet.resume(&foreign).err().unwrap().kind,
            TransactionErrorKind::InvalidSnapshot
        );
    }

    #[test]
    fn transfers_reject_foreign_network_addresses_and_excess_precision() {
        let (_, wallet) = wallet(Node::default());
        let mut builder = TransferBuilder::new(&wallet);
        let mainnet = base::Address::new(
            Network::Mainnet.canonical(),
            "1BitcoinEaterAddressDontSendf59kuE",
        );
        let error = builder.transfer(mainnet, "1".parse().unwrap()).unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidAddress);
        let error = builder
            .transfer(own(&builder), "0.000000001".parse().unwrap())
            .unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidAmount);
    }
}

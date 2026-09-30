//! [`TransferBuilder`]: the `base::TransactionBuilder` of an Ethereum wallet,
//! with its snapshot encoding.

use base::{
    Decimal, Draft, TransactionBuilder, TransactionError, TransactionErrorKind, TransactionFuture,
    TransactionSnapshot, Transfer,
};
use serde::Deserialize;

use crate::native::{Address, AssetKind, ETH, Error, SignedTransaction, Wei};
use crate::wallet::Wallet;

/// Builds one transfer from one Ethereum wallet: set it, snapshot it, prepare
/// (reserve a nonce, preflight, sign) it through the wallet's outbox.
pub struct TransferBuilder {
    wallet: Wallet,
    transfer: Option<(Address, Decimal)>,
}

impl TransferBuilder {
    /// Snapshot kind of an unsigned transfer.
    pub const SNAPSHOT_KIND: &str = "ethereum.transfer";
    /// Kind of a prepared (signed) transaction.
    pub const PREPARED_KIND: &str = "ethereum.signed";

    pub(crate) fn new(wallet: &Wallet) -> Self {
        Self {
            wallet: wallet.clone(),
            transfer: None,
        }
    }

    /// The native signed transaction inside a prepared SDK transaction.
    pub(crate) fn decode(
        prepared: &base::SignedTransaction,
    ) -> Result<SignedTransaction, TransactionError> {
        let invalid = |message: String| {
            TransactionError::new(TransactionErrorKind::InvalidTransaction, message)
        };
        if prepared.version() != base::SignedTransaction::VERSION
            || prepared.kind() != Self::PREPARED_KIND
        {
            return Err(invalid(
                "prepared transaction is not an Ethereum signed envelope".to_owned(),
            ));
        }
        let id = prepared
            .id()
            .as_str()
            .parse()
            .map_err(|error: Error| invalid(error.message))?;
        SignedTransaction::from_envelope(id, prepared.envelope().clone())
            .map_err(|error| invalid(error.message))
    }

    /// Rebuilds a builder from a snapshot this wallet produced.
    pub(crate) fn restore(
        wallet: &Wallet,
        snapshot: &TransactionSnapshot,
    ) -> Result<Self, TransactionError> {
        let mut builder = Self::new(wallet);
        builder.transfer = Some(builder.decode_snapshot(snapshot)?);
        builder.validate()?;
        Ok(builder)
    }

    fn from(&self) -> &Address {
        self.wallet.key.address()
    }

    fn validate(&self) -> Result<(), TransactionError> {
        self.wallet.config.validate().map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidSnapshot, error.message)
        })
    }

    fn configured(&self) -> Result<&(Address, Decimal), TransactionError> {
        self.transfer.as_ref().ok_or_else(|| {
            TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "transfer is not configured",
            )
        })
    }

    /// The chain-owned durable encoding of this builder, inside its
    /// [`Draft`].
    fn snapshot(&self) -> Result<TransactionSnapshot, TransactionError> {
        self.validate()?;
        let (destination, amount) = self.configured()?;
        let config = &self.wallet.config;
        let asset = match &config.asset {
            AssetKind::Native => serde_json::json!({
                "kind": "native",
                "ticker": ETH.ticker,
                "decimals": config.decimals,
            }),
            AssetKind::Erc20(token) => serde_json::json!({
                "kind": "erc20",
                "token": token.to_string(),
                "decimals": config.decimals,
            }),
        };
        Ok(TransactionSnapshot::new(
            Self::SNAPSHOT_KIND,
            serde_json::json!({
                "network": {
                    "chain": config.network.chain.as_str(),
                    "name": config.network.name.as_str(),
                },
                "source": self.from().to_string(),
                "to": destination.to_string(),
                "amount": amount.to_string(),
                "asset": asset,
            }),
        ))
    }

    /// What these exact signed bytes may cost: `gas_limit * max_fee_per_gas`,
    /// in wei, decoded from the envelope itself so the figure is the one that
    /// was signed.
    fn fee(signed: &SignedTransaction) -> Result<Decimal, TransactionError> {
        signed
            .inspect_eip1559_fees()
            .and_then(|fees| fees.maximum_total_fee())
            .map(|fee| fee.atomic())
            .map_err(|error| {
                TransactionError::new(TransactionErrorKind::InvalidTransaction, error.message)
            })
    }

    fn decode_snapshot(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<(Address, Decimal), TransactionError> {
        let config = &self.wallet.config;
        if snapshot.version() != TransactionSnapshot::VERSION
            || snapshot.kind() != Self::SNAPSHOT_KIND
        {
            return Err(invalid("snapshot is not a supported Ethereum transfer"));
        }
        let body: SnapshotBody = serde_json::from_value(snapshot.value().clone())
            .map_err(|error| invalid(format!("invalid Ethereum snapshot: {error}")))?;
        if body.network.chain != config.network.chain.as_str()
            || body.network.name != config.network.name
            || body.network.chain != crate::CHAIN_NAME.as_str()
            || config.chain_id == 0
            || body.source != self.from().to_string()
            || !body.asset.matches(&config.asset, config.decimals)
        {
            return Err(invalid(
                "Ethereum snapshot does not belong to this wallet, network, or asset",
            ));
        }
        let destination = body
            .to
            .parse::<Address>()
            .map_err(|error| invalid(error.message))?;
        let amount = body.amount.parse::<Decimal>().map_err(invalid)?;
        Wei::from_decimal(&amount, config.decimals).map_err(invalid)?;
        Ok((destination, amount))
    }
}

impl TransactionBuilder for TransferBuilder {
    fn transfer(
        &mut self,
        destination: base::Address,
        amount: Decimal,
    ) -> Result<(), TransactionError> {
        if self.transfer.is_some() {
            return Err(TransactionError::new(
                TransactionErrorKind::Unsupported,
                "Ethereum transaction builder supports exactly one transfer",
            ));
        }
        let destination = Address::try_from(&destination).map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidAddress, error.message)
        })?;
        Wei::from_decimal(&amount, self.wallet.config.decimals).map_err(|error| {
            TransactionError::new(TransactionErrorKind::InvalidAmount, error.message)
        })?;
        self.transfer = Some((destination, amount));
        Ok(())
    }

    fn draft(&self) -> Result<Draft, TransactionError> {
        let network = &self.wallet.config.network;
        let (destination, amount) = self.configured()?;
        Ok(Draft {
            source: self.from().canonical(network),
            transfers: vec![Transfer {
                to: destination.canonical(network),
                amount: amount.clone(),
            }],
            asset: self.wallet.config.asset.id(),
            snapshot: self.snapshot()?,
        })
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            self.validate()?;
            let (destination, amount) = self.configured()?.clone();
            let request = self.wallet.request(destination, &amount).map_err(|error| {
                TransactionError::new(TransactionErrorKind::InvalidAmount, error.message)
            })?;
            let (signed, nonce) = self.wallet.outbox.sign(request).await?;
            let fee = Self::fee(&signed)?;
            let claim = base::Claim::new([format!(
                "{}/{nonce}",
                self.from().canonical(&self.wallet.config.network)
            )]);
            Ok(base::SignedTransaction::new(
                Self::PREPARED_KIND,
                (&signed.id).into(),
                signed.envelope,
                fee,
            )
            .with_claim(claim))
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotBody {
    network: NetworkBody,
    source: String,
    to: String,
    amount: String,
    asset: Asset,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NetworkBody {
    chain: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum Asset {
    Native { ticker: String, decimals: u32 },
    Erc20 { token: String, decimals: u32 },
}

impl Asset {
    fn matches(&self, configured: &AssetKind, decimals: u32) -> bool {
        match (self, configured) {
            (
                Self::Native {
                    ticker,
                    decimals: actual,
                },
                AssetKind::Native,
            ) => ticker == ETH.ticker && *actual == decimals,
            (
                Self::Erc20 {
                    token,
                    decimals: actual,
                },
                AssetKind::Erc20(configured),
            ) => token == &configured.to_string() && *actual == decimals,
            _ => false,
        }
    }
}

/// An unusable snapshot; the message says why.
fn invalid(error: impl std::fmt::Display) -> TransactionError {
    TransactionError::new(TransactionErrorKind::InvalidSnapshot, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::Networked;
    use wallets::{Author, FactoryProfile as _};

    use super::*;
    use crate::native::{KeyPair, Network};
    use crate::sender::stub::{CHAIN_ID, Node, signer};
    use crate::wallet::WalletConfig;

    const SNAPSHOT_KIND: &str = TransferBuilder::SNAPSHOT_KIND;

    fn config() -> WalletConfig {
        WalletConfig::new(&Network::new(11_155_111, "sepolia"))
    }

    fn builder(config: &WalletConfig, source: &Address) -> TransferBuilder {
        let key = KeyPair {
            pair: crypto::KeyPair::secp256k1(vec![1; 32]).unwrap(),
            address: source.clone(),
        };
        let wallet = Wallet::fixture(config.clone(), key, Arc::new(Node::default()));
        TransferBuilder::new(&wallet)
    }

    fn decode(
        config: &WalletConfig,
        source: &Address,
        snapshot: &TransactionSnapshot,
    ) -> Result<(Address, Decimal), TransactionError> {
        builder(config, source).decode_snapshot(snapshot)
    }

    fn snapshot(source: &Address) -> TransactionSnapshot {
        TransactionSnapshot::new(
            SNAPSHOT_KIND,
            serde_json::json!({
                "network": { "chain": "ethereum", "name": "sepolia" },
                "source": source.to_string(),
                "to": Address::from_bytes([0x22; 20]).to_string(),
                "amount": "1.25",
                "asset": { "kind": "native", "ticker": "ETH", "decimals": 18 },
            }),
        )
    }

    #[test]
    fn invalid_snapshot_adapter_preserves_validation_order_and_exact_errors() {
        let wallet = Address::from_bytes([0x11; 20]);
        let unsupported = TransactionSnapshot::new("unsupported", serde_json::json!({}));
        let error = decode(&config(), &wallet, &unsupported)
            .expect_err("kind must be checked before malformed content");
        assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot);
        assert_eq!(
            error.message,
            "snapshot is not a supported Ethereum transfer"
        );
        assert_eq!(error.ambiguous_transaction_id, None);

        for (destination, amount, message) in [
            (
                "invalid".to_owned(),
                "invalid",
                "Ethereum address is missing its 0x prefix",
            ),
            (
                Address::from_bytes([0x22; 20]).to_string(),
                "invalid",
                "decimal must use canonical base-10 notation",
            ),
            (
                Address::from_bytes([0x22; 20]).to_string(),
                "-1",
                "currency amount must not be negative",
            ),
        ] {
            let mut value = snapshot(&wallet).value().clone();
            value["to"] = serde_json::json!(destination);
            value["amount"] = serde_json::json!(amount);
            let error = decode(
                &config(),
                &wallet,
                &TransactionSnapshot::new(SNAPSHOT_KIND, value),
            )
            .expect_err("invalid snapshot contents must be classified before restoration");
            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot);
            assert_eq!(error.message, message);
            assert_eq!(error.ambiguous_transaction_id, None);
        }
    }

    #[test]
    fn json_round_trip_restores_transfer_intent() {
        let wallet = Address::from_bytes([0x11; 20]);
        let encoded = serde_json::to_string(&snapshot(&wallet)).expect("snapshot must serialize");
        let restored = serde_json::from_str(&encoded).expect("snapshot must deserialize");
        let (destination, amount) = decode(&config(), &wallet, &restored)
            .expect("matching wallet must restore its transaction");

        assert_eq!(destination, Address::from_bytes([0x22; 20]));
        assert_eq!(amount.to_string(), "1.25");
        assert!(!encoded.contains("private"));
    }

    #[test]
    fn erc20_json_round_trip_restores_transfer_intent() {
        let wallet = Address::from_bytes([0x11; 20]);
        let token = Address::from_bytes([0xab; 20]);
        let mut config = config();
        config.asset = AssetKind::Erc20(token.clone());
        config.decimals = 6;
        let mut value = snapshot(&wallet).value().clone();
        value["asset"] = serde_json::json!({
            "kind": "erc20", "token": token.to_string(), "decimals": 6,
        });
        let encoded = serde_json::to_string(&TransactionSnapshot::new(SNAPSHOT_KIND, value))
            .expect("snapshot must serialize");
        let restored = serde_json::from_str(&encoded).expect("snapshot must deserialize");

        let (destination, amount) = decode(&config, &wallet, &restored)
            .expect("matching token wallet must restore its transaction");

        assert_eq!(destination, Address::from_bytes([0x22; 20]));
        assert_eq!(amount.to_string(), "1.25");
    }

    #[test]
    fn native_snapshots_reject_asset_mismatches() {
        let wallet = Address::from_bytes([0x11; 20]);
        for (case, asset) in [
            (
                "ticker",
                serde_json::json!({ "kind": "native", "ticker": "BTC", "decimals": 18 }),
            ),
            (
                "ticker casing",
                serde_json::json!({ "kind": "native", "ticker": "eth", "decimals": 18 }),
            ),
            (
                "decimals",
                serde_json::json!({ "kind": "native", "ticker": "ETH", "decimals": 17 }),
            ),
            (
                "token variant",
                serde_json::json!({
                    "kind": "erc20",
                    "token": Address::from_bytes([0xab; 20]).to_string(),
                    "decimals": 18,
                }),
            ),
        ] {
            let mut value = snapshot(&wallet).value().clone();
            value["asset"] = asset;
            let error = decode(
                &config(),
                &wallet,
                &TransactionSnapshot::new(SNAPSHOT_KIND, value),
            )
            .expect_err(case);

            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot, "{case}");
            assert_eq!(
                error.message,
                "Ethereum snapshot does not belong to this wallet, network, or asset",
                "{case}"
            );
        }
    }

    #[test]
    fn erc20_snapshots_reject_asset_mismatches() {
        let wallet = Address::from_bytes([0x11; 20]);
        let token = Address::from_bytes([0xab; 20]);
        let mut config = config();
        config.asset = AssetKind::Erc20(token.clone());
        config.decimals = 6;
        for (case, asset) in [
            (
                "token contract",
                serde_json::json!({
                    "kind": "erc20",
                    "token": Address::from_bytes([0xcd; 20]).to_string(),
                    "decimals": 6,
                }),
            ),
            (
                "token casing",
                serde_json::json!({
                    "kind": "erc20",
                    "token": format!("0x{}","AB".repeat(20)),
                    "decimals": 6,
                }),
            ),
            (
                "decimals",
                serde_json::json!({ "kind": "erc20", "token": token.to_string(), "decimals": 18 }),
            ),
            (
                "native variant",
                serde_json::json!({ "kind": "native", "ticker": "ETH", "decimals": 6 }),
            ),
        ] {
            let mut value = snapshot(&wallet).value().clone();
            value["asset"] = asset;
            let error = decode(
                &config,
                &wallet,
                &TransactionSnapshot::new(SNAPSHOT_KIND, value),
            )
            .expect_err(case);

            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot, "{case}");
            assert_eq!(
                error.message,
                "Ethereum snapshot does not belong to this wallet, network, or asset",
                "{case}"
            );
        }
    }

    #[test]
    fn wallet_config_rejects_a_zero_token_contract() {
        let mut config = config();
        config.asset = AssetKind::Erc20(Address::from_bytes([0; 20]));
        config.decimals = 6;

        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_kind_version_network_chain_and_wallet_mismatches() {
        let wallet = Address::from_bytes([0x11; 20]);
        assert!(
            decode(
                &config(),
                &wallet,
                &TransactionSnapshot::new("bitcoin.transfer", serde_json::json!({}))
            )
            .is_err()
        );

        let mut encoded = serde_json::to_value(snapshot(&wallet)).expect("snapshot must serialize");
        encoded["version"] = serde_json::json!(2);
        let wrong_version = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_version).is_err());

        let mut encoded = serde_json::to_value(snapshot(&wallet)).expect("snapshot must serialize");
        encoded["value"]["network"]["chain"] = serde_json::json!("bitcoin");
        let wrong_chain = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_chain).is_err());

        let mut encoded = serde_json::to_value(snapshot(&wallet)).expect("snapshot must serialize");
        encoded["value"]["network"]["name"] = serde_json::json!("mainnet");
        let wrong_network = serde_json::from_value(encoded).expect("snapshot must deserialize");
        assert!(decode(&config(), &wallet, &wrong_network).is_err());
        assert!(
            decode(
                &config(),
                &Address::from_bytes([0x33; 20]),
                &snapshot(&wallet)
            )
            .is_err()
        );
    }

    /// One prepared transfer of one wei from `wallet` to `to`.
    async fn prepared(wallet: &dyn Author, to: &base::Address) -> base::SignedTransaction {
        let mut builder = wallet.transaction();
        builder
            .transfer(to.clone(), "0.000000000000000001".parse().unwrap())
            .unwrap();
        builder.prepare().await.expect("the transfer must sign")
    }

    #[tokio::test]
    async fn a_signed_transaction_claims_the_nonce_its_outbox_took() {
        let node = Arc::new(Node::default());
        let factory = node.factory();
        let network = Network::new(CHAIN_ID, "local").canonical();
        let destination = Address::from_bytes([0x55; 20]).canonical(&network);
        let authorship = factory.authorship().unwrap();
        let attached = |key| authorship.author(&factory.attach(key).unwrap()).unwrap();
        let mine = attached(signer(21));
        let theirs = attached(signer(22));

        let first = prepared(mine.as_ref(), &destination).await;
        let again = prepared(mine.as_ref(), &destination).await;
        let other = prepared(theirs.as_ref(), &destination).await;

        assert_eq!(
            first.claim(),
            Some(&base::Claim::new([format!("{}/0", mine.address())])),
            "it names the exact nonce the outbox took"
        );
        let (first, again, other) = (
            first.claim().unwrap(),
            again.claim().unwrap(),
            other.claim().unwrap(),
        );
        assert!(
            first.collides_with(again),
            "two in-flight sends of one wallet take the same nonce"
        );
        assert!(!first.collides_with(other), "another wallet's does not");
    }
}

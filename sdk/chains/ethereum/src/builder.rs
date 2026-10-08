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

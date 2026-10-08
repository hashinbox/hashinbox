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
use crate::sender::SendRequest;
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

    /// One send request per configured transfer, all from this wallet.
    fn configured(&self) -> Result<Vec<SendRequest>, TransactionError> {
        if self.transfers.is_empty() {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "transaction has no recipients",
            ));
        }
        self.transfers
            .iter()
            .map(|(to, amount)| {
                self.wallet.request(to.clone(), amount).map_err(|error| {
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
            .map(|(to, amount)| {
                serde_json::json!({
                    "to": to.as_str(),
                    "amount": amount.to_string(),
                })
            })
            .collect::<Vec<_>>();
        Ok(TransactionSnapshot::new(
            Self::SNAPSHOT_KIND,
            serde_json::json!({
                "network": {
                    "chain": self.wallet.config.network.chain.as_str(),
                    "name": self.wallet.config.network.name.as_str(),
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
            || body.network.name != self.wallet.config.network.name
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
                let to = Address::parse(&transfer.to, self.wallet.config.native)
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
    name: String,
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
    to: String,
    amount: String,
}

//! [`TransferBuilder`]: the `base::TransactionBuilder` of a Solana wallet,
//! with its snapshot encoding.

use base::{
    Decimal, Draft, TransactionBuilder, TransactionError, TransactionErrorKind, TransactionFuture,
    TransactionSnapshot, Transfer,
};
use serde::Deserialize;
use wallets::SendError;

use crate::error::send_failure;
use crate::native::{Address, AssetKind, CHAIN_NAME, Lamport, SOL, TransactionId};
use crate::wallet::Wallet;

/// Builds one native SOL transfer from one Solana wallet: set it, snapshot
/// it, prepare (reserve the sender, read accounts and a blockhash, check
/// funds, sign, simulate) it. The prepared transaction must be broadcast by a
/// wallet of the same factory within its blockhash lifetime (about a minute).
///
/// SOL is the only asset this chain authors, and the only one whose factory
/// declares a `wallets::Authorship`. A token wallet is never a
/// `wallets::Author`, so it reaches no builder at all and this file has no
/// asset to refuse: every draft and snapshot below is native by
/// construction.
pub struct TransferBuilder {
    wallet: Wallet,
    transfer: Option<(Address, Decimal)>,
}

impl TransferBuilder {
    /// Snapshot kind of an unsigned transfer.
    pub const SNAPSHOT_KIND: &str = "solana.transfer";
    /// Kind of a prepared (signed) transaction.
    pub const PREPARED_KIND: &str = "solana.signed";

    pub(crate) fn new(wallet: &Wallet) -> Self {
        Self {
            wallet: wallet.clone(),
            transfer: None,
        }
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

    /// The transaction ID of a prepared SDK transaction of this chain.
    pub(crate) fn decode(
        prepared: &base::SignedTransaction,
    ) -> Result<TransactionId, TransactionError> {
        if prepared.version() != base::SignedTransaction::VERSION
            || prepared.kind() != Self::PREPARED_KIND
        {
            return Err(TransactionError::new(
                TransactionErrorKind::InvalidTransaction,
                "prepared transaction is not a Solana signed envelope",
            ));
        }
        Ok(prepared.id().as_str().parse()?)
    }

    fn from(&self) -> &Address {
        self.wallet.key.address()
    }

    /// Refuses a wallet whose configuration is unusable, before any snapshot
    /// is written or any send is prepared — the same check at the same three
    /// points as bitcoin's and ethereum's builders.
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
        let network = &self.wallet.config.network;
        Ok(TransactionSnapshot::new(
            Self::SNAPSHOT_KIND,
            serde_json::json!({
                "network": {
                    "chain": network.chain.as_str(),
                    "name": network.name.as_str(),
                },
                "source": self.from().to_string(),
                "to": destination.to_string(),
                "amount": amount.to_string(),
                "asset": {"kind": "native", "ticker": SOL.ticker, "decimals": SOL.decimals},
            }),
        ))
    }

    fn decode_snapshot(
        &self,
        snapshot: &TransactionSnapshot,
    ) -> Result<(Address, Decimal), TransactionError> {
        if snapshot.version() != TransactionSnapshot::VERSION
            || snapshot.kind() != Self::SNAPSHOT_KIND
        {
            return Err(invalid("snapshot is not a supported Solana transfer"));
        }
        let body: SnapshotBody = serde_json::from_value(snapshot.value().clone())
            .map_err(|error| invalid(format!("invalid Solana snapshot: {error}")))?;
        let network = &self.wallet.config.network;
        if body.network.chain != CHAIN_NAME.as_str()
            || body.network.name != network.name
            || body.source != self.from().to_string()
            || !body.asset.is_sol()
        {
            return Err(invalid(
                "Solana snapshot does not belong to this wallet, network, or asset",
            ));
        }
        let destination = body
            .to
            .parse::<Address>()
            .map_err(|error| invalid(error.message))?;
        let amount = body.amount.parse::<Decimal>().map_err(invalid)?;
        Lamport::from_decimal(&amount, self.wallet.config.decimals)
            .map_err(|error| invalid(error.message))?;
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
                "Solana transaction builder supports exactly one transfer",
            ));
        }
        let destination = Address::try_from(&destination)?;
        Lamport::from_decimal(&amount, self.wallet.config.decimals).map_err(|error| {
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
            asset: AssetKind::Native.id(),
            snapshot: self.snapshot()?,
        })
    }

    fn prepare<'a>(
        &'a mut self,
    ) -> TransactionFuture<'a, Result<base::SignedTransaction, TransactionError>> {
        Box::pin(async move {
            self.validate()?;
            let (destination, amount) = self.configured()?.clone();
            let request = self
                .wallet
                .request(destination, &amount)
                .map_err(|error| send_failure(SendError::item(0, Vec::new(), error)))?;
            let (signed, fee) = self.wallet.outbox.sign(request).await?;
            let claim = base::Claim::new([signed
                .from
                .canonical(&self.wallet.config.network)
                .to_string()]);
            Ok(base::SignedTransaction::new(
                Self::PREPARED_KIND,
                signed.id.into(),
                signed.envelope,
                fee.atomic(),
            )
            .with_claim(claim))
        })
    }
}

/// An unusable snapshot; the message says why.
fn invalid(error: impl std::fmt::Display) -> TransactionError {
    TransactionError::new(TransactionErrorKind::InvalidSnapshot, error.to_string())
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
#[serde(deny_unknown_fields)]
struct Asset {
    kind: String,
    ticker: String,
    decimals: u32,
}

impl Asset {
    fn is_sol(&self) -> bool {
        self.kind == "native" && self.ticker == SOL.ticker && self.decimals == SOL.decimals
    }
}

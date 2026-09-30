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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::SecretBytes;

    use super::*;
    use crate::native::rpc::{
        Client,
        script::{Script, Scripted},
    };
    use crate::sender::stub::{self, Node, Registrar, key};

    async fn wallet(node: &Node, registrar: Arc<Registrar>) -> Arc<dyn wallets::Author> {
        let client = Client::unverified(node.clone(), Script::network());
        let factory = stub::factory(client, registrar);
        factory.authored(SecretBytes::new([7; 32])).await
    }

    /// Every entry point of this builder checks the wallet's configuration, as
    /// bitcoin's and ethereum's do. Before it did, `WalletConfig::validate`
    /// was called from four other places and from no builder at all.
    #[tokio::test]
    async fn every_entry_point_refuses_a_wallet_whose_configuration_is_unusable() {
        let client = Client::unverified(Node::default(), Script::network());
        let factory = stub::factory(client, Registrar::accepting());
        let sound = factory.attached(key(7));
        let to = key(9).address().canonical(&sound.config.network);
        let snapshot = {
            let mut builder = TransferBuilder::new(&sound);
            builder.transfer(to.clone(), "1".parse().unwrap()).unwrap();
            builder.draft().expect("a sound wallet drafts").snapshot
        };

        let mut wallet = factory.attached(key(7));
        wallet.config.network.name = " ".to_owned();

        let refused = TransferBuilder::restore(&wallet, &snapshot)
            .err()
            .expect("restore checks the configuration");
        assert_eq!(refused.kind, TransactionErrorKind::InvalidSnapshot);
        let mut builder = TransferBuilder::new(&wallet);
        builder
            .transfer(to, "1".parse().unwrap())
            .expect("the transfer itself is well formed");
        assert_eq!(
            builder
                .draft()
                .expect_err("draft checks the configuration")
                .kind,
            TransactionErrorKind::InvalidSnapshot
        );
        assert_eq!(
            builder
                .prepare()
                .await
                .expect_err("prepare checks the configuration before any chain call")
                .kind,
            TransactionErrorKind::InvalidSnapshot
        );
    }

    /// One prepared transfer of one SOL from `wallet` to `to`.
    async fn prepared(wallet: &dyn wallets::Author, to: &base::Address) -> base::SignedTransaction {
        let mut builder = wallet.transaction();
        builder.transfer(to.clone(), "1".parse().unwrap()).unwrap();
        builder.prepare().await.expect("the transfer must sign")
    }

    #[tokio::test]
    async fn a_signed_transaction_claims_the_payer_its_sends_are_ordered_through() {
        let client = Client::unverified(Node::default(), Script::network());
        let factory = stub::factory(client, Registrar::accepting());
        let mine = factory.authored(SecretBytes::new([7; 32])).await;
        let theirs = factory.authored(SecretBytes::new([8; 32])).await;
        let to = key(9).address().canonical(&mine.address().network);

        let first = prepared(mine.as_ref(), &to).await;
        let again = prepared(mine.as_ref(), &to).await;
        let other = prepared(theirs.as_ref(), &to).await;

        assert_eq!(
            first.claim(),
            Some(&base::Claim::new([mine.address().to_string()])),
            "it names the payer its sends are ordered through"
        );
        let (first, again, other) = (
            first.claim().unwrap(),
            again.claim().unwrap(),
            other.claim().unwrap(),
        );
        assert!(
            first.collides_with(again),
            "two in-flight sends of one wallet order through one payer"
        );
        assert!(!first.collides_with(other), "another wallet's does not");
    }

    #[tokio::test]
    async fn one_transfer_snapshots_restores_and_rejects_a_second_or_invalid_one() {
        let wallet = wallet(&Node::default(), Registrar::accepting()).await;
        let mut builder = wallet.transaction();
        let to = key(8).address().canonical(&wallet.address().network);
        let error = builder
            .transfer(
                base::Address::new(wallet.address().network.clone(), "0"),
                Decimal::from(1_u64),
            )
            .unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidAddress);
        let error = builder
            .transfer(to.clone(), "0.0000000001".parse().unwrap())
            .unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::InvalidAmount);
        builder
            .transfer(to.clone(), "1.25".parse().unwrap())
            .unwrap();
        let error = builder.transfer(to, Decimal::from(1_u64)).unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::Unsupported);

        let draft = builder.draft().unwrap();
        assert_eq!(draft.source, *wallet.address());
        assert_eq!(draft.asset, AssetKind::Native.id());
        assert_eq!(draft.transfers.len(), 1);
        assert_eq!(draft.transfers[0].amount.to_string(), "1.25");
        let snapshot = draft.snapshot;
        assert_eq!(
            snapshot.value()["asset"],
            serde_json::json!({"kind": "native", "ticker": "SOL", "decimals": 9})
        );
        let restored = wallet.resume(&snapshot).unwrap();
        assert_eq!(restored.draft().unwrap().snapshot, snapshot);
    }

    #[tokio::test]
    async fn snapshots_bind_wallet_network_and_asset() {
        let wallet = wallet(&Node::default(), Registrar::accepting()).await;
        let mut builder = wallet.transaction();
        builder
            .transfer(
                key(8).address().canonical(&wallet.address().network),
                "1".parse().unwrap(),
            )
            .unwrap();
        let snapshot = builder.draft().unwrap().snapshot;
        let mut other = snapshot.value().clone();
        for (field, value) in [
            ("/network/name", serde_json::json!("mainnet-beta")),
            ("/network/chain", serde_json::json!("ethereum")),
            ("/source", serde_json::json!(key(9).address().to_string())),
            ("/asset/decimals", serde_json::json!(18)),
            ("/to", serde_json::json!("bad")),
            ("/amount", serde_json::json!("0")),
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
            assert_eq!(error.kind, TransactionErrorKind::InvalidSnapshot, "{field}");
            *other.pointer_mut(field).unwrap() = original;
        }
        let foreign = TransactionSnapshot::new("bitcoin.transfer", serde_json::json!({}));
        assert_eq!(
            wallet.resume(&foreign).err().unwrap().kind,
            TransactionErrorKind::InvalidSnapshot
        );
        assert_eq!(
            wallet.transaction().draft().unwrap_err().kind,
            TransactionErrorKind::InvalidTransaction
        );
    }

    /// The defect this split removed. A token wallet and a native wallet of
    /// one key share an address, so a token wallet used to reach this builder
    /// twice over — through a new builder, and through `resume` of the native
    /// wallet's own snapshot — and draft native SOL under the token's tag.
    /// Four runtime refusals guarded that. None is needed now: an SPL factory
    /// declares no `wallets::Authorship`, so the map refuses its tag before a
    /// builder exists, and what it does hand out is an `Arc<dyn
    /// wallets::Wallet>` with no `transaction`, `resume` or `send` on it at
    /// all — proved at compile time by the `compile_fail` examples on
    /// `wallets::Wallet`.
    #[tokio::test]
    async fn a_token_asset_is_refused_before_a_builder_exists() {
        let client = Client::unverified(Node::default(), Script::network());
        let factory = stub::factory(client, Registrar::accepting());
        let token = factory
            .sibling(
                crate::WalletConfig::new(&Script::network())
                    .with_token(crate::native::Address::from_bytes([9; 32]), 6),
            )
            .expect("a token sibling of one cluster");
        let held = wallets::WalletFactory::create(&token, SecretBytes::new([7; 32]))
            .await
            .expect("an SPL holding still has a wallet, a balance and a history");
        let native = factory.authored(SecretBytes::new([7; 32])).await;
        let to = key(8).address().canonical(&held.address().network);

        let mut map = wallets::Wallets::new();
        map.add("usdc", token).expect("one factory per asset");
        let Err(refused) = map.author(&"usdc", &held) else {
            panic!("an SPL holding must author nothing")
        };

        assert_eq!(refused.kind, wallets::WalletErrorKind::Unsupported);
        assert_eq!(held.address(), native.address(), "one key, one address");
        let mut builder = native.transaction();
        builder.transfer(to, "1".parse().unwrap()).unwrap();
        let draft = builder.draft().expect("the native wallet still drafts");
        assert_eq!(draft.asset, AssetKind::Native.id());
        // Its snapshot has nowhere else to go: resuming it needs an author,
        // and the token asset has none to resume it on.
        assert!(map.author(&"usdc", &held).is_err());
    }

    #[tokio::test]
    async fn a_prepared_transaction_broadcasts_once_through_its_wallet() {
        let node = Node::default();
        let registrar = Registrar::accepting();
        let wallet = wallet(&node, registrar.clone()).await;
        let mut builder = wallet.transaction();
        builder
            .transfer(
                key(8).address().canonical(&wallet.address().network),
                "0.001".parse().unwrap(),
            )
            .unwrap();
        let prepared = builder.prepare().await.expect("prepared");
        assert_eq!(prepared.kind(), TransferBuilder::PREPARED_KIND);

        let broadcasting = wallet.clone();
        let sent = prepared.clone();
        let waiter = tokio::spawn(async move { broadcasting.broadcaster().broadcast(&sent).await });
        tokio::task::yield_now().await;
        registrar.task().run().await;
        let submission = waiter.await.unwrap().expect("broadcast");
        assert_eq!(&submission.id, prepared.id());

        let again = wallet.broadcaster().broadcast(&prepared).await;
        assert_eq!(
            again.unwrap_err().kind,
            TransactionErrorKind::InvalidTransaction
        );
        let foreign = base::SignedTransaction::new(
            "ethereum.signed",
            prepared.id().clone(),
            prepared.envelope().clone(),
            prepared.fee().clone(),
        );
        let error = wallet.broadcaster().broadcast(&foreign).await.unwrap_err();
        assert_eq!(
            (error.kind, error.ambiguous_transaction_id),
            (TransactionErrorKind::InvalidTransaction, None)
        );
    }
}

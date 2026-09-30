//! `wallets::Sender`: one batch of transfers through the factory's
//! [`Outbox`], one transaction per transfer, submitted in order and stopped
//! at the first failure.

mod dispatch;
mod outbox;
mod recovery;
mod reservation;
#[cfg(test)]
pub(crate) mod stub;

use std::{any::Any, sync::Arc};

use wallets::{
    Author, Authorship, SendError, SendFuture, Sender, Transfer, WalletError, WalletErrorKind,
};

pub(crate) use outbox::{Outbox, SendRequest};

use crate::native::{Address, Wei, transaction};
use crate::wallet::{Wallet, WalletConfig};

/// One factory's `wallets::Sender` and `wallets::Authorship`: its
/// configuration and the outbox it shares with its siblings. It resolves
/// every transfer before any chain call, then hands the batch to the outbox,
/// which reserves each sender for the whole batch.
pub(crate) struct FactorySender {
    config: WalletConfig,
    outbox: Arc<Outbox>,
}

impl FactorySender {
    pub(crate) const fn new(config: WalletConfig, outbox: Arc<Outbox>) -> Self {
        Self { config, outbox }
    }

    /// The send request of one authored transfer; the wallet signs it.
    fn request(&self, transfer: Transfer) -> Result<SendRequest, WalletError> {
        let from = Address::try_from(transfer.wallet.address())?;
        let to = Address::try_from(&transfer.to)?;
        let amount = Wei::from_decimal(&transfer.amount, self.config.decimals)
            .map_err(|error| WalletError::new(WalletErrorKind::InvalidAmount, error.message))?;
        let native = transaction::Transfer {
            asset: self.config.asset.clone(),
            to,
            amount,
        };
        Ok(SendRequest::new(
            transfer.wallet,
            from,
            native,
            self.config.fee_limits.clone(),
        ))
    }

    async fn execute(
        &self,
        transfers: Vec<Transfer>,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        SendError::check_count(transfers.len())?;
        let items = transfers
            .into_iter()
            .enumerate()
            .map(|(index, transfer)| {
                self.request(transfer)
                    .map_err(|error| SendError::item(index, Vec::new(), error))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.outbox.send(items).await
    }
}

impl Sender for FactorySender {
    fn send<'a>(&'a self, transfers: Vec<Transfer>) -> SendFuture<'a> {
        Box::pin(self.execute(transfers))
    }
}

impl Authorship for FactorySender {
    /// The wallet's own authoring capability, recovered by the one type that
    /// makes it. Membership is `WalletFactory::validate`'s answer, which
    /// `wallets::Wallets::author` has already taken; this repeats only the
    /// two facts a send pipeline needs — one configuration, one outbox.
    fn author(&self, wallet: &Arc<dyn wallets::Wallet>) -> Result<Arc<dyn Author>, WalletError> {
        let wallet = (wallet.as_ref() as &dyn Any)
            .downcast_ref::<Wallet>()
            .filter(|wallet| {
                wallet.config == self.config && Arc::ptr_eq(&wallet.outbox, &self.outbox)
            })
            .ok_or_else(|| {
                WalletError::new(
                    WalletErrorKind::Unsupported,
                    "wallet was not created by this asset's factory",
                )
            })?;
        Ok(Arc::new(wallet.clone()))
    }
}

#[cfg(test)]
mod tests {
    use base::SecretBytes;
    use futures_executor::block_on;
    use wallets::{FactoryProfile, MAX_TRANSFERS, WalletFactory as _};

    use super::*;
    use crate::sender::stub::{Action, Node};

    fn direct_sender(node: &Arc<Node>) -> (Arc<dyn Sender>, Arc<dyn wallets::Wallet>) {
        let factory = node.factory();
        let wallet = block_on(factory.create(SecretBytes::new([1_u8; 32])))
            .expect("fixed valid secret must create an Ethereum wallet");
        (factory.authorship().unwrap(), wallet)
    }

    fn transfer(wallet: Arc<dyn wallets::Wallet>) -> Transfer {
        Transfer {
            to: wallet.address().clone(),
            wallet,
            amount: "0.000000000000000001"
                .parse()
                .expect("one wei must be a valid decimal"),
        }
    }

    fn assert_no_chain_io(node: &Node) {
        assert!(node.balance_reads.lock().unwrap().is_empty());
        assert!(node.nonce_reads.lock().unwrap().is_empty());
        assert!(node.contexts.lock().unwrap().is_empty());
        assert!(node.broadcasts.lock().unwrap().is_empty());
    }

    fn assert_invalid_batch(failure: SendError, message: &str) {
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.failed_index, None);
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.source.kind, WalletErrorKind::InvalidBatch);
        assert_eq!(failure.source.message, message);
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(failure.to_string(), message);
    }

    #[test]
    fn direct_sender_rejects_an_empty_batch_before_chain_io() {
        let node = Arc::new(Node::default());
        let (sender, _) = direct_sender(&node);

        let failure = block_on(sender.send(Vec::new()))
            .expect_err("the concrete Ethereum sender must reject an empty batch");

        assert_invalid_batch(failure, "at least one transfer is required");
        assert_no_chain_io(&node);
    }

    #[test]
    fn direct_sender_rejects_51_items_before_chain_io() {
        let node = Arc::new(Node::default());
        let (sender, wallet) = direct_sender(&node);
        let transfers = (0..=MAX_TRANSFERS)
            .map(|_| transfer(wallet.clone()))
            .collect();

        let failure = block_on(sender.send(transfers))
            .expect_err("the concrete Ethereum sender must reject 51 items");

        assert_invalid_batch(failure, "at most 50 transfers are allowed");
        assert_no_chain_io(&node);
    }

    #[test]
    fn item_preflight_reports_the_first_authored_error_before_chain_io() {
        let node = Arc::new(Node::default());
        let (sender, wallet) = direct_sender(&node);
        let first = transfer(wallet.clone());
        let mut second = transfer(wallet.clone());
        second.amount = base::Decimal::zero();
        let mut third = transfer(wallet);
        third.to.value = "not-an-address".to_owned();

        let failure = block_on(sender.send(vec![first, second, third]))
            .expect_err("the earlier invalid amount must win before any chain I/O");

        assert_eq!(failure.failed_index, Some(1));
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.source.kind, WalletErrorKind::InvalidAmount);
        assert_eq!(failure.source.message, "Ethereum amount must be positive");
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_no_chain_io(&node);
    }

    #[test]
    fn duplicates_keep_authored_indices_and_stop_after_the_ambiguous_item() {
        let node = Arc::new(Node::default());
        node.actions([
            Action::Accept,
            Action::Ambiguous("provider claimed transaction provider-candidate"),
        ]);
        let (sender, wallet) = direct_sender(&node);
        let transfers = (0..3).map(|_| transfer(wallet.clone())).collect();

        let failure = block_on(sender.send(transfers))
            .expect_err("the second exact occurrence must fail ambiguously");
        let nonces = node.nonces_used();
        let broadcasts = node.broadcasts.lock().unwrap().clone();

        assert_eq!(nonces, [0, 1, 2]);
        assert_eq!(broadcasts.len(), 2);
        assert_ne!(broadcasts[0].id, broadcasts[1].id);
        assert_eq!(
            failure.accepted,
            [base::TransactionId::from(&broadcasts[0].id)]
        );
        assert_eq!(failure.failed_index, Some(1));
        assert_eq!(
            failure.ambiguous_transaction_id,
            Some(base::TransactionId::from(&broadcasts[1].id))
        );
        assert_eq!(failure.source.ambiguous_transaction_id, None);
        assert_eq!(failure.source.kind, WalletErrorKind::Unavailable);
        assert_eq!(
            failure.source.message,
            "provider claimed transaction provider-candidate"
        );
    }

    #[test]
    fn preparation_failure_preserves_index_with_zero_accepted() {
        let node = Arc::new(Node::default());
        let (sender, wallet) = direct_sender(&node);
        let from = Address::try_from(wallet.address()).unwrap();
        node.set_balance(&from, &crate::native::AssetKind::Native, 1);

        let failure = block_on(sender.send(vec![transfer(wallet.clone()), transfer(wallet)]))
            .expect_err("two transfers plus fees exceed one wei");

        assert_eq!(failure.failed_index, Some(0));
        assert!(failure.accepted.is_empty());
        assert_eq!(failure.source.kind, WalletErrorKind::Transaction);
        assert_eq!(
            failure.source.message,
            "Ethereum aggregate batch balance is insufficient"
        );
        assert_eq!(failure.ambiguous_transaction_id, None);
        assert!(node.broadcasts.lock().unwrap().is_empty());
    }

    #[test]
    fn broadcast_failure_preserves_transaction_classification_for_http_mapping() {
        let unavailable = WalletError::from(base::TransactionError::new(
            base::TransactionErrorKind::Unavailable,
            "submission outcome is ambiguous",
        ));
        let rejected = WalletError::from(base::TransactionError::new(
            base::TransactionErrorKind::Rejected,
            "node rejected the transaction",
        ));

        assert_eq!(unavailable.kind, WalletErrorKind::Unavailable);
        assert_eq!(rejected.kind, WalletErrorKind::Transaction);
    }
}

//! `wallets::Sender`: one batch of native SOL transfers through the
//! factory's [`outbox::Outbox`], one transaction per transfer, submitted in
//! order and stopped at the first failure.

mod dispatch;
mod outbox;
mod recovery;
mod registration;
mod reservation;
#[cfg(test)]
pub(crate) mod stub;

use std::{any::Any, sync::Arc};

use wallets::{
    Author, Authorship, SendError, SendFuture, Sender, Transfer, WalletError, WalletErrorKind,
};

pub(crate) use outbox::{Outbox, SendRequest};
pub(crate) use recovery::{CommitSignal, Recovery};
pub(crate) use registration::Spawner;
pub(crate) use reservation::Reservations;

use base::Decimal;

use crate::WalletConfig;
use crate::native::{Address, Lamport, transaction};
use crate::wallet::Wallet;

/// One factory's `wallets::Sender` and `wallets::Authorship`: its
/// configuration and the outbox it shares with its siblings. It resolves
/// every transfer before any chain call, then hands the batch to the outbox,
/// which reserves each sender for the whole batch.
///
/// Only a native SOL factory builds one: an SPL factory declares no
/// `wallets::Authorship`, so a token batch does not exist to refuse.
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
        let amount = match transfer.amount {
            wallets::Amount::Exact(amount) => amount,
            wallets::Amount::All => return SendRequest::sweep(transfer.wallet, from, to),
        };
        let native = native_transfer(&from, to, &amount, self.config.decimals)?;
        Ok(SendRequest::new(transfer.wallet, from, native))
    }

    async fn execute(
        &self,
        transfers: Vec<Transfer>,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        SendError::check_count(transfers.len())?;
        SendError::check_selection(&transfers)?;
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

/// The native SOL transfer of `amount` from `from` to `to`, read in
/// `decimals`.
///
/// The scale is an argument because the amount type takes one: SOL's nine
/// belongs to the caller's `WalletConfig`, not to this function.
///
/// # Errors
///
/// `Unsupported` for an off-curve destination, `AddressMismatch` when `to`
/// is the sender, and `InvalidAmount` unless `amount` is a positive number of
/// atomic units at that scale.
pub(crate) fn native_transfer(
    from: &Address,
    to: Address,
    amount: &Decimal,
    decimals: u32,
) -> Result<transaction::Transfer, WalletError> {
    destination(from, &to)?;
    let amount = Lamport::from_decimal(amount, decimals).map_err(|_| {
        WalletError::new(
            WalletErrorKind::InvalidAmount,
            "native SOL amount is invalid",
        )
    })?;
    Ok(transaction::Transfer::new(to, amount)?)
}

/// The native send boundary, shared by exact transfers and unresolved sweeps.
fn destination(from: &Address, to: &Address) -> Result<(), WalletError> {
    if !to.is_on_curve() {
        return Err(WalletError::new(
            WalletErrorKind::Unsupported,
            "unsupported Solana native destination",
        ));
    }
    if to == from {
        return Err(WalletError::new(
            WalletErrorKind::AddressMismatch,
            "Solana source and destination must differ",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use base::SecretBytes;
    use wallets::{FactoryProfile, MAX_TRANSFERS};

    use super::*;
    use crate::native::SOL;
    use crate::native::rpc::{
        Client,
        script::{Script, Scripted},
    };
    use crate::sender::stub::{Node, factory, key};

    fn node_factory(node: &Node) -> crate::WalletFactory {
        let client = Client::unverified(node.clone(), Script::network());
        factory(client, Arc::new(Spawner))
    }

    fn destination(wallet: &Arc<dyn Author>) -> base::Address {
        key(8).address().canonical(&wallet.address().network)
    }

    fn transfers(wallet: &Arc<dyn Author>, count: usize) -> Vec<Transfer> {
        let to = destination(wallet);
        (0..count)
            .map(|_| Transfer {
                wallet: wallet.clone(),
                to: to.clone(),
                amount: "0.00001".parse().unwrap(),
            })
            .collect()
    }

    #[test]
    fn native_transfers_reject_off_curve_self_and_invalid_amounts_before_any_chain_call() {
        let from = key(7).address().clone();
        let pda = "2fnQrngrQT4SeLcdToJAD96phoEjNL2man2kfRLCASVk"
            .parse::<Address>()
            .unwrap();
        for (to, amount, kind, message) in [
            (
                pda,
                "1",
                WalletErrorKind::Unsupported,
                "unsupported Solana native destination",
            ),
            (
                from.clone(),
                "1",
                WalletErrorKind::AddressMismatch,
                "Solana source and destination must differ",
            ),
            (
                key(8).address().clone(),
                "0",
                WalletErrorKind::InvalidAmount,
                "native SOL amount is invalid",
            ),
            (
                key(8).address().clone(),
                "0.0000000001",
                WalletErrorKind::InvalidAmount,
                "native SOL amount is invalid",
            ),
        ] {
            let error =
                native_transfer(&from, to, &amount.parse().unwrap(), SOL.decimals).unwrap_err();
            assert_eq!((error.kind, error.message.as_str()), (kind, message));
        }
        let transfer = native_transfer(
            &from,
            key(8).address().clone(),
            &"1.000000001".parse().unwrap(),
            SOL.decimals,
        )
        .unwrap();
        assert_eq!(transfer.amount, Lamport::from_atomic(1_000_000_001));
    }

    fn sends(node: &Node) -> usize {
        let methods = node.methods.lock().unwrap();
        methods
            .iter()
            .filter(|method| *method == "sendTransaction")
            .count()
    }

    #[tokio::test]
    async fn routes_one_and_fifty_items_and_rejects_zero_and_fifty_one_first() {
        let node = Node::default();
        let factory = node_factory(&node);
        let wallet = factory.authored(SecretBytes::new([7; 32])).await;
        let sender = factory.authorship().unwrap();
        for count in [1, MAX_TRANSFERS] {
            let ids = sender
                .send(transfers(&wallet, count))
                .await
                .expect("valid batch");
            assert_eq!(ids.len(), count);
        }
        assert_eq!(sends(&node), 1 + MAX_TRANSFERS);
        node.methods.lock().unwrap().clear();
        for count in [0, MAX_TRANSFERS + 1] {
            let error = sender.send(transfers(&wallet, count)).await.unwrap_err();
            assert_eq!(error.source.kind, WalletErrorKind::InvalidBatch);
        }
        assert!(node.methods.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn items_resolve_before_any_chain_call_and_name_the_failing_item() {
        let node = Node::default();
        let factory = node_factory(&node);
        let wallet = factory.authored(SecretBytes::new([7; 32])).await;
        let mut batch = transfers(&wallet, 2);
        batch[1].to.value = format!("0x{}", batch[1].to.value);
        let error = factory.authorship().unwrap().send(batch).await.unwrap_err();
        assert_eq!(error.failed_index, Some(1));
        assert_eq!(error.source.kind, WalletErrorKind::InvalidAddress);
        let mut batch = transfers(&wallet, 2);
        batch[0].amount = Decimal::zero().into();
        let error = factory.authorship().unwrap().send(batch).await.unwrap_err();
        assert_eq!(
            (error.failed_index, error.source.kind),
            (Some(0), WalletErrorKind::InvalidAmount)
        );
        assert!(node.methods.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn single_and_batch_paths_share_one_sender_reservation_in_both_directions() {
        for single_first in [true, false] {
            let node = Node {
                lost: true,
                ..Node::default()
            };
            let factory = node_factory(&node);
            let wallet = factory.authored(SecretBytes::new([7; 32])).await;
            let batch = factory.authorship().unwrap();
            let to = destination(&wallet);
            let amount: Decimal = "0.00001".parse().unwrap();
            let first_ambiguous = if single_first {
                wallet
                    .send(to.clone(), amount.clone().into())
                    .await
                    .expect_err("single ambiguity")
                    .ambiguous_transaction_id
            } else {
                batch
                    .send(transfers(&wallet, 1))
                    .await
                    .expect_err("batch ambiguity")
                    .ambiguous_transaction_id
            };
            assert!(first_ambiguous.is_some());
            let second = if single_first {
                batch
                    .send(transfers(&wallet, 1))
                    .await
                    .expect_err("batch must see it")
                    .source
            } else {
                wallet
                    .send(to, amount.into())
                    .await
                    .expect_err("single must see it")
            };
            assert_eq!(second.kind, WalletErrorKind::SourceBusy);
        }
    }
}

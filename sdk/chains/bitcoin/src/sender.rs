//! `wallets::Sender`: one batch of transfers as one transaction through the
//! factory's [`Outbox`], each sender paying its own transfers, change, and
//! share of the fee.

mod dispatch;
mod outbox;
mod recovery;
mod reservation;
#[cfg(test)]
pub(crate) mod stub;
mod utxos;

use std::{any::Any, sync::Arc};

use wallets::{
    Author, Authorship, SendError, SendFuture, Sender, Transfer, WalletError, WalletErrorKind,
};

pub(crate) use outbox::{Outbox, SendRequest};
pub(crate) use utxos::UtxoReader;

use crate::WalletConfig;
use crate::native::{Address, Satoshi, transaction};
use crate::wallet::Wallet;

/// One factory's `wallets::Sender` and `wallets::Authorship`: its
/// configuration and the outbox its wallets share. It resolves every transfer
/// before any chain call, then hands the batch to the outbox, which reserves
/// the outputs it spends.
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
        let to = self.config.destination(&transfer.to)?;
        match transfer.amount {
            wallets::Amount::Exact(amount) => {
                let amount = Satoshi::from_decimal(&amount).map_err(|error| {
                    WalletError::new(WalletErrorKind::InvalidAmount, error.message)
                })?;
                Ok(SendRequest::new(
                    transfer.wallet,
                    from,
                    transaction::Transfer::new(to, amount),
                ))
            }
            wallets::Amount::All => Ok(SendRequest::sweep(transfer.wallet, from, to)),
        }
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

#[cfg(test)]
mod tests {
    use base::Networked;
    use bitcoin::{ScriptBuf, consensus};
    use futures_executor::block_on;
    use indexing::{IndexError, IndexErrorKind};
    use wallets::{FactoryProfile, MAX_TRANSFERS, Wallet, WalletFactory as _};

    use super::*;
    use crate::native::{AddressType, Error, Network, transaction::FeeRate};
    use crate::sender::stub::Node;

    fn address(byte: u8) -> Address {
        Node::key(byte).address(Network::Regtest, AddressType::SegwitV0)
    }

    fn transfers(
        wallets: &[&Arc<dyn Wallet>],
        to: &Arc<dyn Wallet>,
        amount: &str,
    ) -> Vec<Transfer> {
        let destination = to.address().clone();
        wallets
            .iter()
            .map(|wallet| Transfer {
                wallet: (*wallet).clone(),
                to: destination.clone(),
                amount: amount.parse().unwrap(),
            })
            .collect()
    }

    fn assert_operation(failure: &SendError, kind: WalletErrorKind, message: &str) {
        assert!(failure.accepted.is_empty());
        assert_eq!(
            (
                failure.failed_index,
                failure.ambiguous_transaction_id.clone()
            ),
            (None, None)
        );
        assert_eq!(
            (failure.source.kind, failure.source.message.as_str()),
            (kind, message)
        );
    }

    #[test]
    fn invalid_batches_fail_before_chain_io() {
        let node = Arc::new(Node::default());
        let factory = node.factory();
        let wallet = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
        let sender = factory.authorship().unwrap();
        let failure = block_on(sender.send(Vec::new())).unwrap_err();
        assert_operation(
            &failure,
            WalletErrorKind::InvalidBatch,
            "at least one transfer is required",
        );
        let many = transfers(&vec![&wallet; MAX_TRANSFERS + 1], &wallet, "0.00000001");
        let failure = block_on(sender.send(many)).unwrap_err();
        assert_operation(
            &failure,
            WalletErrorKind::InvalidBatch,
            "at most 50 transfers are allowed",
        );
        assert!(node.events().is_empty());
    }

    #[test]
    fn fee_failures_stop_before_output_reads() {
        for (fee, kind, message) in [
            (
                Err(Error::rpc("fee estimate unavailable", true)),
                WalletErrorKind::Unavailable,
                "fee estimate unavailable",
            ),
            (
                Ok(FeeRate::from_sat_per_vb(10_001)),
                WalletErrorKind::Transaction,
                "estimated Bitcoin fee rate exceeds the configured maximum",
            ),
        ] {
            let node = Arc::new(Node {
                fee,
                ..Node::default()
            });
            let factory = node.factory();
            let wallet = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
            let failure = block_on(factory.authorship().unwrap().send(transfers(
                &[&wallet],
                &wallet,
                "0.00001",
            )))
            .unwrap_err();
            assert_operation(&failure, kind, message);
            assert_eq!(node.events(), ["fees"]);
        }
    }

    #[test]
    fn sources_are_read_in_first_seen_order_at_one_checkpoint() {
        let node = Arc::new(Node {
            outputs: Err(IndexError::new(IndexErrorKind::Conflict, "moved", true)),
            ..Node::default()
        });
        let factory = node.factory();
        let first = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
        let second = block_on(factory.create(crypto::SecretBytes::new([2; 32]))).unwrap();
        let batch = transfers(&[&first, &second, &first], &first, "0.00001");
        let failure = block_on(factory.authorship().unwrap().send(batch)).unwrap_err();
        assert_operation(&failure, WalletErrorKind::Unavailable, "moved");
        assert_eq!(node.events(), ["fees", address(1).as_str()]);
    }

    #[test]
    fn sweep_spends_every_free_output_without_change() {
        let alice = address(1);
        let node = Arc::new(Node {
            outputs: Ok(vec![(alice.clone(), 100_000), (alice, 100_000)]),
            ..Node::default()
        });
        let factory = node.factory();
        let wallet = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
        let to = address(2).canonical(&Network::Regtest.canonical());
        let batch = vec![Transfer {
            wallet,
            to: to.clone(),
            amount: wallets::Amount::All,
        }];
        block_on(factory.authorship().unwrap().send(batch)).unwrap();
        let submitted = node.submitted.lock().unwrap();
        let transaction: bitcoin::Transaction =
            consensus::deserialize(submitted[0].envelope.as_bytes()).unwrap();
        assert_eq!(transaction.input.len(), 2);
        assert_eq!(transaction.output.len(), 1);
        assert_eq!(transaction.output[0].value.to_sat(), 199_822);
        assert_eq!(
            transaction.output[0].script_pubkey,
            address(2).script_pubkey(Network::Regtest).unwrap()
        );
    }

    #[test]
    fn sweep_excludes_outputs_reserved_by_an_accepted_send() {
        let alice = address(1);
        let node = Arc::new(Node {
            outputs: Ok(vec![(alice.clone(), 100_000), (alice, 100_000)]),
            ..Node::default()
        });
        let factory = node.factory();
        let wallet = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
        let to = address(2).canonical(&Network::Regtest.canonical());
        let sender = factory.authorship().unwrap();
        block_on(sender.send(vec![Transfer {
            wallet: wallet.clone(),
            to: to.clone(),
            amount: "0.00001".parse().unwrap(),
        }]))
        .unwrap();
        block_on(sender.send(vec![Transfer {
            wallet,
            to,
            amount: wallets::Amount::All,
        }]))
        .unwrap();
        let submitted = node.submitted.lock().unwrap();
        let first: bitcoin::Transaction =
            consensus::deserialize(submitted[0].envelope.as_bytes()).unwrap();
        let sweep: bitcoin::Transaction =
            consensus::deserialize(submitted[1].envelope.as_bytes()).unwrap();
        assert_eq!(sweep.input.len(), 1);
        assert_ne!(
            first.input[0].previous_output,
            sweep.input[0].previous_output
        );
        assert_eq!(sweep.output.len(), 1);
        assert_eq!(sweep.output[0].value.to_sat(), 99_890);
    }

    #[test]
    fn an_empty_sweep_is_an_invalid_amount_without_submission() {
        let node = Arc::new(Node::default());
        let factory = node.factory();
        let wallet = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
        let to = address(2).canonical(&Network::Regtest.canonical());
        let failure = block_on(factory.authorship().unwrap().send(vec![Transfer {
            wallet,
            to,
            amount: wallets::Amount::All,
        }]))
        .unwrap_err();
        assert_eq!(failure.source.kind, WalletErrorKind::InvalidAmount);
        assert!(failure.source.message.contains("no available UTXOs"));
        assert!(node.submitted.lock().unwrap().is_empty());
    }

    #[test]
    fn mixed_sweep_keeps_fixed_payments_and_never_drops_a_dust_remainder() {
        for total in [100_000, 1_001] {
            let node = Arc::new(Node {
                outputs: Ok(vec![(address(1), total)]),
                ..Node::default()
            });
            let factory = node.factory();
            let wallet = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
            let to = address(2).canonical(&Network::Regtest.canonical());
            let batch = vec![
                Transfer {
                    wallet: wallet.clone(),
                    to: to.clone(),
                    amount: "0.00001".parse().unwrap(),
                },
                Transfer {
                    wallet,
                    to,
                    amount: wallets::Amount::All,
                },
            ];
            let sent = block_on(factory.authorship().unwrap().send(batch));
            if total == 1_001 {
                assert!(sent.is_err());
                assert!(node.submitted.lock().unwrap().is_empty());
                continue;
            }
            sent.unwrap();
            let submitted = node.submitted.lock().unwrap();
            let transaction: bitcoin::Transaction =
                consensus::deserialize(submitted[0].envelope.as_bytes()).unwrap();
            assert_eq!(
                transaction
                    .output
                    .iter()
                    .map(|output| output.value.to_sat())
                    .collect::<Vec<_>>(),
                [1_000, 98_859]
            );
        }
    }

    #[test]
    fn grouped_submission_signs_each_input_with_its_owner() {
        let (alice, bob) = (address(1), address(2));
        let node = Arc::new(Node {
            outputs: Ok(vec![
                (alice.clone(), 100_000),
                (alice.clone(), 100_000),
                (bob.clone(), 100_000),
            ]),
            ..Node::default()
        });
        let factory = node.factory();
        let first = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
        let second = block_on(factory.create(crypto::SecretBytes::new([2; 32]))).unwrap();
        let batch = transfers(&[&first, &second, &first], &first, "0.00001");
        let accepted = block_on(factory.authorship().unwrap().send(batch)).unwrap();
        let submitted = node.submitted.lock().unwrap();
        let transaction: bitcoin::Transaction =
            consensus::deserialize(submitted[0].envelope.as_bytes()).unwrap();
        assert_eq!(
            accepted,
            vec![base::TransactionId::new(
                transaction.compute_txid().to_string()
            )]
        );
        assert_eq!(transaction.input.len(), 3);
        for (input, owner) in transaction.input.iter().zip([&alice, &alice, &bob]) {
            let public =
                bitcoin::PublicKey::from_slice(input.witness.iter().nth(1).unwrap()).unwrap();
            let script = ScriptBuf::new_p2wpkh(&public.wpubkey_hash().unwrap());
            assert_eq!(script, owner.script_pubkey(Network::Regtest).unwrap());
        }
        let recipient = alice.script_pubkey(Network::Regtest).unwrap();
        let paid = transaction
            .output
            .iter()
            .filter(|output| output.value.to_sat() == 1_000 && output.script_pubkey == recipient)
            .count();
        assert_eq!(paid, 3);
        for owner in [&alice, &bob] {
            let script = owner.script_pubkey(Network::Regtest).unwrap();
            let change = transaction
                .output
                .iter()
                .filter(|output| output.value.to_sat() > 1_000 && output.script_pubkey == script)
                .count();
            assert_eq!(change, 1);
        }
    }

    #[test]
    fn an_unknown_broadcast_outcome_names_the_grouped_transaction() {
        let alice = address(1);
        let node = Arc::new(Node {
            outputs: Ok(vec![(alice, 100_000)]),
            broadcast: Some(Error::rpc("timed out", true)),
            ..Node::default()
        });
        let factory = node.factory();
        let wallet = block_on(factory.create(crypto::SecretBytes::new([1; 32]))).unwrap();
        let failure = block_on(factory.authorship().unwrap().send(transfers(
            &[&wallet],
            &wallet,
            "0.00001",
        )))
        .unwrap_err();
        let submitted = node.submitted.lock().unwrap();
        assert_eq!(
            failure.ambiguous_transaction_id,
            Some(submitted[0].id.into())
        );
        assert_eq!(failure.source.ambiguous_transaction_id, None);
    }
}

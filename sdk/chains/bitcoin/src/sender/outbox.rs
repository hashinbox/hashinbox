//! [`Outbox`]: every send of one factory's wallets, in five stages:
//! prepare → reserve → sign → submit → settle.
//!
//! - **prepare**: the node's fee rate for the configured target, refused
//!   above the ceiling,
//! - **reserve**: settle any unknown outcome of the senders first, read their
//!   indexed outputs at one checkpoint, and select from the outputs no other
//!   send holds, atomically with reserving the selection. One sender selects
//!   its largest outputs first ([`Builder`]); several senders each spend all
//!   their free outputs in one grouped transaction ([`Builder::grouped`]
//!   over one [`Funds`] per sender),
//! - **sign** each input with its owner, then ask the node's mempool
//!   (`testmempoolaccept`) whether it would take the result,
//! - **submit** the one transaction,
//! - **settle**: acceptance keeps the outputs reserved until their spend is
//!   indexed, a definite refusal frees them, and an unknown outcome keeps
//!   them for [`super::recovery::Recovery`].

use std::sync::Arc;

use base::{Signer, TransactionError, TransactionErrorKind};
use wallets::SendError;

use super::{
    dispatch::Dispatch,
    recovery::Recovery,
    reservation::{Admission, Reservations},
    utxos::UtxoReader,
};
use crate::native::{
    Address, Error, Network, Satoshi, SignedTransaction, TransactionId,
    block::{Outpoint, Utxo},
    rpc::Submitter,
    transaction::{
        BuildContext, Builder, FeeRate, Funds, Preflight, Transfer, UnsignedTransaction,
    },
};
use crate::wallet::WalletConfig;

/// The send pipeline of one factory: the only place that reserves
/// outputs, signs, and submits.
pub(crate) struct Outbox {
    network: Network,
    fee_target_blocks: u16,
    max_fee_rate: FeeRate,
    preflight: Arc<dyn Preflight>,
    utxos: Arc<UtxoReader>,
    dispatch: Dispatch,
    recovery: Recovery,
    reservations: Reservations,
}

/// One transfer, its sender, and the signer that controls the sender.
pub(crate) struct Item {
    from: Address,
    transfer: Transfer,
    signer: Arc<dyn Signer>,
}

/// One send signed and ready to submit: the signed bytes, the fee they pay,
/// the outputs they spend, and the reservation that selected them.
///
/// Solana's `sender::outbox` names the same stage's product `SignedBatch`; a
/// Bitcoin send is one transaction however many senders pay it, so the batch
/// here is one set of bytes.
struct SignedBatch {
    transaction: SignedTransaction,
    fee: Satoshi,
    spends: Vec<Outpoint>,
    admission: Admission,
}

/// One sender of a send and the transfers it pays, in first-seen order.
struct Payer {
    address: Address,
    signer: Arc<dyn Signer>,
    transfers: Vec<Transfer>,
}

impl Item {
    /// `transfer` from `from`, signed by `signer`.
    pub(crate) fn new(signer: Arc<dyn Signer>, from: Address, transfer: Transfer) -> Self {
        Self {
            from,
            transfer,
            signer,
        }
    }
}

impl Outbox {
    /// A pipeline with `config`'s network and fee policy over the node's
    /// preflight and submission and the index's outputs, with no
    /// reservations yet.
    pub(crate) fn new(
        config: &WalletConfig,
        preflight: Arc<dyn Preflight>,
        submitter: Arc<dyn Submitter>,
        utxos: Arc<UtxoReader>,
    ) -> Self {
        let dispatch = Dispatch {
            submitter: submitter.clone(),
            max_fee_rate: config.max_fee_rate,
        };
        Self {
            network: config.native,
            fee_target_blocks: config.fee_target_blocks,
            max_fee_rate: config.max_fee_rate,
            preflight,
            utxos,
            recovery: Recovery {
                submitter,
                dispatch: dispatch.clone(),
            },
            dispatch,
            reservations: Reservations::default(),
        }
    }

    /// Sends `items` as one transaction and returns its ID.
    ///
    /// # Errors
    ///
    /// A grouped failure: one transaction pays every item, so no item index.
    /// When the outcome is unknown, it carries the local transaction ID.
    pub(crate) async fn send(
        &self,
        items: Vec<Item>,
    ) -> Result<Vec<base::TransactionId>, SendError> {
        let failed = |error: TransactionError| SendError::grouped(Vec::new(), error.into());
        let SignedBatch {
            transaction,
            admission,
            ..
        } = self.prepare(items).await.map_err(failed)?;
        let id = self
            .dispatch
            .run(admission.settlement(transaction))
            .await
            .map_err(failed)?;
        Ok(vec![id.into()])
    }

    /// Prepares, reserves, and signs one sender's transfers, then releases
    /// the outputs and holds the transaction for [`Self::broadcast`]. A held
    /// transaction never blocks its outputs; if a later send takes one
    /// first, its broadcast fails definitely.
    ///
    /// Returns the signed transaction, the fee it pays — what its selected
    /// inputs hold minus what its outputs, change included, pay out — and
    /// the outputs it spends, which is what a caller comparing two signed
    /// transactions needs and cannot read off the envelope.
    pub(crate) async fn sign(
        &self,
        items: Vec<Item>,
    ) -> Result<(SignedTransaction, Satoshi, Vec<Outpoint>), TransactionError> {
        let SignedBatch {
            transaction,
            fee,
            spends,
            admission,
        } = self.prepare(items).await?;
        self.reservations.hold(admission, transaction.clone());
        Ok((transaction, fee, spends))
    }

    /// Submits and settles a transaction [`Self::sign`] produced, or settles
    /// one whose earlier broadcast had an unknown outcome.
    ///
    /// # Errors
    ///
    /// A definite failure when this outbox holds no exact `transaction`, or
    /// another send took its outputs; otherwise the submission's outcome.
    pub(crate) async fn broadcast(
        &self,
        transaction: SignedTransaction,
    ) -> Result<TransactionId, TransactionError> {
        if let Some(held) = self.reservations.take(&transaction) {
            let (transaction, admission) =
                self.reservations.readmit(held).map_err(Error::definite)?;
            return self.dispatch.run(admission.settlement(transaction)).await;
        }
        match self.reservations.settlement(&transaction) {
            Some(settlement) => self.recovery.settle(settlement).await,
            None => Err(Error::rpc(
                "Bitcoin transaction was not prepared by this wallet factory or was settled",
                false,
            )
            .definite()),
        }
    }

    /// Prepare, reserve, and sign: nothing leaves the process until the
    /// node's mempool would accept the signed transaction.
    async fn prepare(&self, items: Vec<Item>) -> Result<SignedBatch, TransactionError> {
        let payers = payers(items);
        let addresses = payers
            .iter()
            .map(|payer| payer.address.clone())
            .collect::<Vec<_>>();
        let fee_rate = self
            .preflight
            .fee_rate(self.fee_target_blocks, self.max_fee_rate)
            .await?;
        self.recover(&addresses).await;
        let read = self.utxos.read(&addresses).await.map_err(|error| {
            TransactionError::new(TransactionErrorKind::Unavailable, error.message)
        })?;
        let (unsigned, admission) = self.reservations.admit(&addresses, read, |available| {
            self.select(&payers, available, fee_rate)
        })?;
        let fee = unsigned.fee()?;
        let spends = unsigned
            .inputs
            .iter()
            .map(|spend| spend.utxo.outpoint)
            .collect();
        let transaction = self.sign_each(&payers, unsigned).await?;
        self.preflight
            .accept(&transaction, self.max_fee_rate)
            .await?
            .require()?;
        Ok(SignedBatch {
            transaction,
            fee,
            spends,
            admission,
        })
    }

    /// Settles the unknown outcomes that hold outputs of `addresses`, so a
    /// transaction the node never took stops holding them. A failed
    /// recovery keeps them reserved and does not block this send.
    async fn recover(&self, addresses: &[Address]) {
        for transaction in self.reservations.unknown(addresses) {
            if let Some(settlement) = self.reservations.settlement(&transaction) {
                self.recovery.settle(settlement).await.ok();
            }
        }
    }

    /// One sender selects largest-first with change; several senders each
    /// spend all their `available` outputs.
    fn select(
        &self,
        payers: &[Payer],
        mut available: Vec<Utxo>,
        fee_rate: FeeRate,
    ) -> Result<UnsignedTransaction, Error> {
        if let [payer] = payers {
            let context = BuildContext {
                network: self.network,
                available,
                change: payer.address.clone(),
                fee_rate,
            };
            return Builder::new(payer.transfers.clone(), context).build();
        }
        let funds = payers
            .iter()
            .map(|payer| {
                let (owned, rest) = available
                    .drain(..)
                    .partition(|utxo| utxo.output.address.as_ref() == Some(&payer.address));
                available = rest;
                Funds {
                    available: owned,
                    transfers: payer.transfers.clone(),
                    change: payer.address.clone(),
                }
            })
            .collect();
        Builder::grouped(self.network, funds, fee_rate)
    }

    /// Signs every input with the signer of the sender that owns it.
    async fn sign_each(
        &self,
        payers: &[Payer],
        unsigned: UnsignedTransaction,
    ) -> Result<SignedTransaction, Error> {
        let owners = unsigned
            .inputs
            .iter()
            .map(|spend| {
                let owner = spend.utxo.output.address.as_ref()?;
                let payer = payers.iter().find(|payer| &payer.address == owner)?;
                Some(payer.signer.as_ref())
            })
            .collect::<Option<Vec<&dyn Signer>>>()
            .ok_or_else(|| {
                Error::invalid_transaction("Bitcoin input has no signer in this send")
            })?;
        unsigned.sign_each(self.network, &owners).await
    }

    /// The shared reservation state; for tests.
    #[cfg(test)]
    pub(crate) const fn reservations(&self) -> &Reservations {
        &self.reservations
    }
}

/// Groups `items` by sender, in first-seen order.
fn payers(items: Vec<Item>) -> Vec<Payer> {
    let mut payers: Vec<Payer> = Vec::new();
    for item in items {
        match payers.iter_mut().find(|payer| payer.address == item.from) {
            Some(payer) => payer.transfers.push(item.transfer),
            None => payers.push(Payer {
                address: item.from,
                signer: item.signer,
                transfers: vec![item.transfer],
            }),
        }
    }
    payers
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::TransactionErrorKind;

    use crate::sender::stub::Node;

    fn inputs(node: &Node, index: usize) -> usize {
        let transaction = node.submitted.lock().unwrap()[index].clone();
        let native: bitcoin::Transaction =
            bitcoin::consensus::deserialize(transaction.envelope.as_bytes()).unwrap();
        native.input.len()
    }

    #[tokio::test]
    async fn one_sender_selects_largest_first_and_several_senders_spend_all() {
        let (alice, bob) = (Node::address(1), Node::address(2));
        let node = Arc::new(Node {
            outputs: Ok(vec![
                (alice.clone(), 50_000),
                (alice.clone(), 100_000),
                (bob.clone(), 100_000),
            ]),
            ..Node::default()
        });
        node.outbox()
            .send(vec![Node::item(1, &bob, 1_000), Node::item(1, &bob, 1_000)])
            .await
            .unwrap();
        assert_eq!(inputs(&node, 0), 1, "one sender takes only what it needs");

        node.outbox()
            .send(vec![
                Node::item(1, &bob, 1_000),
                Node::item(2, &alice, 1_000),
            ])
            .await
            .unwrap();
        assert_eq!(
            inputs(&node, 1),
            3,
            "grouped senders spend every free output"
        );
    }

    #[tokio::test]
    async fn a_signed_transaction_never_broadcast_does_not_hold_its_outputs() {
        let node = Arc::new(Node {
            outputs: Ok(vec![(Node::address(1), 100_000)]),
            ..Node::default()
        });
        let outbox = node.outbox();
        let bob = Node::address(2);
        let (abandoned, fee, spends) = outbox
            .sign(vec![Node::item(1, &bob, 1_000)])
            .await
            .expect("the single transfer must sign");
        assert_eq!(fee.units(), 100_000 - 1_000 - 98_859);
        assert_eq!(
            spends.len(),
            1,
            "it reports the one output it selected, and holds none of it"
        );
        assert!(outbox.reservations().reserved().is_empty());

        outbox
            .send(vec![Node::item(1, &bob, 2_000)])
            .await
            .expect("an unbroadcast signature must not hold the output");
        let error = outbox
            .broadcast(abandoned)
            .await
            .expect_err("the accepted send took the held output");
        assert_eq!(
            (error.kind, error.ambiguous_transaction_id),
            (TransactionErrorKind::Rejected, None)
        );
        assert_eq!(node.submitted.lock().unwrap().len(), 1);
    }
}

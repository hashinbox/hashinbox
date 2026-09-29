//! Settling a transaction whose submission outcome is unknown.
//!
//! Recovery is lazy: it runs on the next broadcast of that transaction, or
//! before the next send from an address it spends from. The node is asked
//! whether it has the exact transaction; if not, the identical bytes are
//! replayed. A replay the node refuses conflicts with the chain, so its
//! outputs are released and the index decides what happened.

use std::sync::Arc;

use base::TransactionError;

use super::{dispatch::Dispatch, reservation::Settlement};
use crate::native::{TransactionId, rpc::Submitter};

/// Settles unknown outcomes: accept what the node has, replay the
/// rest.
pub(super) struct Recovery {
    pub(super) submitter: Arc<dyn Submitter>,
    pub(super) dispatch: Dispatch,
}

impl Recovery {
    /// Settles `settlement`, whose earlier submission outcome is unknown.
    pub(super) async fn settle(
        &self,
        settlement: Settlement,
    ) -> Result<TransactionId, TransactionError> {
        let id = settlement.transaction().id;
        match self.submitter.known(&id).await {
            Ok(true) => Ok(settlement.accept()),
            Ok(false) => self.dispatch.run(settlement).await,
            Err(error) => Err(error.ambiguous(&id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::TransactionErrorKind;

    use crate::native::{Error, ErrorKind, SignedTransaction};
    use crate::sender::stub::{Action, Node};

    /// A regtest node paying alice one output, whose first broadcast has an
    /// unknown outcome.
    fn ambiguous() -> Arc<Node> {
        let node = Arc::new(Node {
            outputs: Ok(vec![(Node::address(1), 100_000)]),
            ..Node::default()
        });
        node.actions
            .lock()
            .unwrap()
            .push_back(Action::Fail(Error::rpc("timed out", true)));
        node
    }

    fn submitted(node: &Node) -> Vec<SignedTransaction> {
        node.submitted.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn an_unknown_outcome_is_accepted_when_the_node_has_it() {
        let node = ambiguous();
        node.known.lock().unwrap().push_back(Ok(true));
        let outbox = node.outbox();
        let error = outbox
            .send(vec![Node::item(1, &Node::address(2), 1_000)])
            .await
            .expect_err("the first broadcast times out");
        let transaction = submitted(&node)[0].clone();
        assert_eq!(error.ambiguous_transaction_id, Some(transaction.id.into()));

        let id = outbox.broadcast(transaction.clone()).await.unwrap();
        assert_eq!(id, transaction.id);
        assert_eq!(
            submitted(&node).len(),
            1,
            "a known transaction is not replayed"
        );
        assert_eq!(outbox.reservations().reserved().len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_outcome_replays_the_exact_bytes() {
        let node = ambiguous();
        let outbox = node.outbox();
        outbox
            .send(vec![Node::item(1, &Node::address(2), 1_000)])
            .await
            .unwrap_err();
        let transaction = submitted(&node)[0].clone();
        let id = outbox.broadcast(transaction.clone()).await.unwrap();
        assert_eq!(id, transaction.id);
        assert_eq!(submitted(&node), [transaction.clone(), transaction]);
        assert_eq!(
            node.events()
                .iter()
                .filter(|event| *event == "known")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_refused_replay_releases_the_outputs_but_stays_ambiguous() {
        let node = ambiguous();
        node.actions
            .lock()
            .unwrap()
            .push_back(Action::Fail(Error::new(ErrorKind::Rejected, "code -25")));
        let outbox = node.outbox();
        outbox
            .send(vec![Node::item(1, &Node::address(2), 1_000)])
            .await
            .unwrap_err();
        let transaction = submitted(&node)[0].clone();
        let error = outbox.broadcast(transaction.clone()).await.unwrap_err();
        assert_eq!(error.kind, TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, Some(transaction.id.into()));
        assert!(outbox.reservations().reserved().is_empty());
    }

    #[tokio::test]
    async fn the_next_send_recovers_an_unknown_outcome_first() {
        let node = ambiguous();
        node.actions
            .lock()
            .unwrap()
            .push_back(Action::Fail(Error::new(ErrorKind::Rejected, "code -25")));
        let outbox = node.outbox();
        let bob = Node::address(2);
        outbox
            .send(vec![Node::item(1, &bob, 1_000)])
            .await
            .unwrap_err();
        outbox
            .send(vec![Node::item(1, &bob, 2_000)])
            .await
            .expect("recovery released the output the unknown send held");
        let events = node.events();
        let known = events.iter().position(|event| event == "known").unwrap();
        let read = events
            .iter()
            .rposition(|event| event == Node::address(1).as_str());
        assert!(read.is_some_and(|read| known < read), "{events:?}");
        assert_eq!(submitted(&node).len(), 3);
    }
}

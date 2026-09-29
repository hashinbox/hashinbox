//! Settling an envelope whose submission outcome is unknown.
//!
//! Recovery is lazy: it runs on the next broadcast of that envelope, or
//! before the next send from its sender, which waits for it. The node is
//! asked whether it knows the exact hash; if not, the identical bytes are
//! replayed, so the retained nonce is never reused by another transaction.

use std::sync::Arc;

use base::TransactionError;

use super::{dispatch::Dispatch, reservation::Settlement};
use crate::native::{Error, TransactionId, rpc::Submitter};

/// Settles unknown outcomes: accept what the node knows, replay
/// the rest.
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
        let id = settlement.id().clone();
        match self.submitter.known(&id).await {
            Ok(true) => settlement.accept().map_err(Error::definite),
            Ok(false) => self.dispatch.run(settlement).await,
            Err(error) => Err(error.ambiguous(&id)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        task::{Context, Poll},
    };

    use base::TransactionErrorKind;

    use crate::native::Error;
    use crate::sender::stub::{Action, Node, item, native, signer};

    #[tokio::test]
    async fn ambiguous_submission_reconciles_and_replays_the_exact_envelope() {
        let key = std::sync::Arc::new(signer(4));
        let node = std::sync::Arc::new(Node::default());
        node.set_nonce(key.address(), 5);
        node.actions([Action::Ambiguous("ambiguous submission"), Action::Accept]);
        node.known([Ok(false)]);
        let outbox = node.outbox();
        let (signed, _) = outbox.sign(item(&key, native(1))).await.unwrap();

        let first = outbox
            .broadcast(signed.clone())
            .await
            .expect_err("first submission must be ambiguous");
        assert_eq!(first.kind, TransactionErrorKind::Unavailable);
        assert_eq!(first.message, "ambiguous submission");
        assert_eq!(first.ambiguous_transaction_id, Some((&signed.id).into()));
        assert_eq!(
            outbox
                .broadcast(signed.clone())
                .await
                .expect("exact replay must resolve"),
            signed.id
        );
        assert_eq!(*node.broadcasts.lock().unwrap(), [signed.clone(), signed]);
        assert_eq!(node.known_ids.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn known_recovery_accepts_without_rebroadcast_and_advances_the_nonce_floor() {
        let key = std::sync::Arc::new(signer(4));
        let node = std::sync::Arc::new(Node::default());
        node.set_nonce(key.address(), 5);
        node.actions([Action::Ambiguous("ambiguous submission")]);
        node.known([Ok(true)]);
        let outbox = node.outbox();
        let (old, _) = outbox.sign(item(&key, native(1))).await.unwrap();
        outbox
            .broadcast(old.clone())
            .await
            .expect_err("initial broadcast must be ambiguous");
        assert_eq!(
            outbox
                .broadcast(old.clone())
                .await
                .expect("known exact hash resolves acceptance without replay"),
            old.id
        );
        assert_eq!(
            node.broadcasts.lock().unwrap().as_slice(),
            std::slice::from_ref(&old)
        );
        assert_eq!(
            node.known_ids.lock().unwrap().as_slice(),
            std::slice::from_ref(&old.id)
        );

        let (new, _) = outbox
            .sign(item(&key, native(2)))
            .await
            .expect("the accepted nonce cannot be reused despite the unchanged RPC nonce");
        assert_eq!(node.nonces_used(), [5, 6]);
        outbox
            .broadcast(new.clone())
            .await
            .expect("the next prepared transaction submits normally");
        assert_eq!(*node.broadcasts.lock().unwrap(), [old, new]);
        assert_eq!(node.known_ids.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn recovery_lookup_failure_preserves_the_message_local_id_and_exact_replay() {
        let key = std::sync::Arc::new(signer(4));
        let node = std::sync::Arc::new(Node::default());
        node.actions([Action::Ambiguous("ambiguous submission"), Action::Accept]);
        node.known([Err(Error::rpc("recovery lookup failed", false)), Ok(false)]);
        let outbox = node.outbox();
        let (signed, _) = outbox.sign(item(&key, native(1))).await.unwrap();
        outbox
            .broadcast(signed.clone())
            .await
            .expect_err("first submission must be ambiguous");

        let error = outbox
            .broadcast(signed.clone())
            .await
            .expect_err("failed lookup must leave the original submission unresolved");
        assert_eq!(error.kind, TransactionErrorKind::Unavailable);
        assert_eq!(error.message, "recovery lookup failed");
        assert_eq!(error.ambiguous_transaction_id, Some((&signed.id).into()));
        assert_eq!(
            node.broadcasts.lock().unwrap().len(),
            1,
            "a failed lookup must not issue another broadcast"
        );

        assert_eq!(
            outbox
                .broadcast(signed.clone())
                .await
                .expect("a later lookup must allow the retained exact envelope to replay"),
            signed.id
        );
        assert_eq!(*node.broadcasts.lock().unwrap(), [signed.clone(), signed]);
    }

    #[tokio::test]
    async fn cancellation_after_submission_starts_retains_unknown_for_exact_recovery() {
        let key = std::sync::Arc::new(signer(5));
        let node = std::sync::Arc::new(Node::default());
        node.actions([Action::Pending, Action::Accept]);
        node.known([Ok(false)]);
        let outbox = node.outbox();
        let (signed, _) = outbox.sign(item(&key, native(1))).await.unwrap();

        let mut submission = Box::pin(outbox.broadcast(signed.clone()));
        let mut context = Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(matches!(
            submission.as_mut().poll(&mut context),
            Poll::Pending
        ));
        drop(submission);

        assert_eq!(
            outbox
                .broadcast(signed.clone())
                .await
                .expect("cancelled submission must recover exact bytes"),
            signed.id
        );
        assert_eq!(*node.broadcasts.lock().unwrap(), [signed.clone(), signed]);
    }

    #[tokio::test]
    async fn a_replay_refused_by_the_node_stays_unknown() {
        let key = std::sync::Arc::new(signer(6));
        let node = std::sync::Arc::new(Node::default());
        node.actions([Action::Ambiguous("ambiguous submission"), Action::Reject]);
        node.known([Ok(false)]);
        let outbox = node.outbox();
        let (signed, _) = outbox.sign(item(&key, native(1))).await.unwrap();
        outbox.broadcast(signed.clone()).await.unwrap_err();
        let error = outbox
            .broadcast(signed.clone())
            .await
            .expect_err("the original may still land, so a refused replay is unknown");
        assert_eq!(error.kind, TransactionErrorKind::Unavailable);
        assert_eq!(error.ambiguous_transaction_id, Some((&signed.id).into()));
    }

    #[tokio::test]
    async fn rpc_backed_outbox_retains_unknown_remote_failure_for_exact_replay() {
        use alloy_primitives::hex;
        use serde_json::{Value, json};

        use crate::native::rpc::script::{CHAIN_ID, Reply, Script, Scripted};
        use crate::sender::Outbox;

        let key = std::sync::Arc::new(signer(10));
        let script = Script::node(vec![
            Reply::ok("eth_getTransactionCount", json!("0x5")),
            Reply::ok("eth_chainId", json!("0x7a69")),
            Reply::ok("eth_estimateGas", json!("0x5208")),
            Reply::ok("eth_maxPriorityFeePerGas", json!("0x1")),
            Reply::ok("eth_getBlockByNumber", json!({"baseFeePerGas": "0x1"})),
            Reply::ok("eth_getBalance", json!("0xde0b6b3a7640000")),
            Reply::ok("eth_getBalance", json!("0xde0b6b3a7640000")),
            Reply::failure(
                "eth_sendRawTransaction",
                -32_000,
                "backend failed after upstream acceptance",
            ),
        ]);
        let rpc = std::sync::Arc::new(script.client());
        let outbox = Outbox::new(CHAIN_ID, rpc.clone(), rpc.clone(), rpc);
        let (signed, _) = outbox.sign(item(&key, native(7))).await.unwrap();
        let error = outbox
            .broadcast(signed.clone())
            .await
            .expect_err("unclassified remote failure must be ambiguous");
        assert_eq!(error.ambiguous_transaction_id, Some((&signed.id).into()));
        script.push([
            Reply::ok("eth_getTransactionByHash", Value::Null),
            Reply::ok("eth_sendRawTransaction", json!(signed.id.to_string())),
        ]);
        assert_eq!(
            outbox
                .broadcast(signed.clone())
                .await
                .expect("reserved exact envelope must replay"),
            signed.id
        );
        let envelope = json!([hex::encode_prefixed(signed.envelope.as_bytes())]);
        let submissions = script
            .requests()
            .into_iter()
            .filter(|(method, _)| method == "eth_sendRawTransaction")
            .map(|(_, params)| params)
            .collect::<Vec<_>>();
        assert_eq!(submissions, [envelope.clone(), envelope]);
    }
}

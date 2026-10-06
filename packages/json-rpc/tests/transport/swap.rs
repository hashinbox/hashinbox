//! A swappable client: later calls move to the replacement, a call in flight
//! finishes where it started, and every method follows the swap.

use std::sync::{Arc, Mutex};

use json_rpc::{Batch, BoxFuture, CallResult, Client, Error, RawJson, Request, Statistics, Swap};
use tokio::sync::oneshot;

/// A node that answers every call with its own name, once `release` fires
/// when it has one, and says which method reached it.
struct Node {
    name: &'static str,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Node {
    fn new(name: &'static str) -> Arc<Self> {
        Arc::new(Self {
            name,
            release: Mutex::new(None),
        })
    }

    fn held(name: &'static str) -> (Arc<Self>, oneshot::Sender<()>) {
        let (release, held) = oneshot::channel();
        let node = Self::new(name);
        *node.release.lock().expect("unpoisoned") = Some(held);
        (node, release)
    }

    fn answer(&self, method: &str) -> BoxFuture<'static, Result<CallResult, Error>> {
        let held = self.release.lock().expect("unpoisoned").take();
        let name = RawJson::from_serializable(&format!("{}:{method}", self.name)).expect("a name");
        Box::pin(async move {
            if let Some(held) = held {
                let _ = held.await;
            }
            Ok(Ok(name))
        })
    }
}

impl Client for Node {
    fn request<'a>(&'a self, _request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        self.answer("request")
    }

    fn batch<'a>(&'a self, _batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        let answer = self.answer("batch");
        Box::pin(async move { Ok(vec![answer.await?]) })
    }

    fn broadcast<'a>(&'a self, _request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        self.answer("broadcast")
    }
}

fn text(result: Result<CallResult, Error>) -> String {
    result
        .expect("a reply")
        .expect("a result")
        .deserialize()
        .expect("a name")
}

fn request() -> Request {
    Request::new("identity", serde_json::json!([]))
}

/// A replacement moves the calls, not the handle: what the holder keeps
/// reaches the new node, and a call already sent finishes on the old one.
#[tokio::test]
async fn a_replacement_moves_later_calls_and_lets_a_call_in_flight_finish() {
    let (first, release) = Node::held("first");
    let swap = Arc::new(Swap::new(first, vec![Statistics::default()]));
    let held: Arc<dyn Client> = swap.clone();
    let in_flight = tokio::spawn({
        let held = held.clone();
        async move { text(held.request(request()).await) }
    });
    tokio::task::yield_now().await;

    swap.replace(Node::new("second"), Vec::new());
    assert_eq!(text(held.request(request()).await), "second:request");
    release.send(()).expect("the first call waits");
    assert_eq!(in_flight.await.expect("the call ends"), "first:request");
}

/// Batches and broadcasts follow the swap too, a broadcast still as a
/// broadcast, and the counters change with the transport they describe.
#[tokio::test]
async fn every_method_and_the_counters_follow_the_swap() {
    let old = Statistics::default();
    let swap = Swap::new(Node::new("first"), vec![old]);
    assert_eq!(swap.statistics().len(), 1);
    assert_eq!(format!("{swap:?}"), "Swap { node_count: 1 }");

    let new = vec![Statistics::default(), Statistics::default()];
    swap.replace(Node::new("second"), new);
    assert_eq!(swap.statistics().len(), 2);
    assert_eq!(text(swap.broadcast(request()).await), "second:broadcast");
    let batch =
        Batch::new(vec![json_rpc::Call::new("identity", serde_json::json!([]))]).expect("a batch");
    let replies = swap.batch(batch).await.expect("replies");
    assert_eq!(
        text(Ok(replies.into_iter().next().expect("one reply"))),
        "second:batch"
    );
}

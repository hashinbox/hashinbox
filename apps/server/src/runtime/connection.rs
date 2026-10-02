//! A replaceable connection. The runtime quiesces whole operations before replacement.
use crate::lock::Lock as _;
use json_rpc::{Batch, BoxFuture, CallResult, Client, Error, Request};
use std::sync::{Arc, Mutex};

pub(crate) struct Connection(Mutex<Arc<dyn Client>>);
impl Connection {
    pub(crate) fn new(client: Arc<dyn Client>) -> Self {
        Self(Mutex::new(client))
    }
    pub(crate) fn replace(&self, client: Arc<dyn Client>) {
        *self.0.hold() = client;
    }
    pub(super) fn snapshot(&self) -> Arc<dyn Client> {
        self.0.hold().clone()
    }
}
impl Client for Connection {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        let client = self.snapshot();
        Box::pin(async move { client.request(request).await })
    }
    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        let client = self.snapshot();
        Box::pin(async move { client.batch(batch).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use json_rpc::RawJson;

    /// A node that answers every call with its own name.
    struct Node(&'static str);

    impl Client for Node {
        fn request<'a>(&'a self, _request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
            let name = RawJson::from_serializable(&self.0).expect("a name");
            Box::pin(async move { Ok(Ok(name)) })
        }
        fn batch<'a>(&'a self, _batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    async fn answered(client: &dyn Client) -> String {
        let reply = client
            .request(Request::once("tip", serde_json::json!([])))
            .await
            .expect("a reply")
            .expect("a result");
        reply.deserialize().expect("a name")
    }

    /// A node switch replaces what answers, not what is held: a sender built
    /// over the connection before the switch, with its reservations and
    /// submissions, reaches the new node through the same handle.
    #[tokio::test]
    async fn a_switch_keeps_the_handle_and_moves_its_calls() {
        let connection = Arc::new(Connection::new(Arc::new(Node("first"))));
        let held: Arc<dyn Client> = connection.clone();
        assert_eq!(answered(held.as_ref()).await, "first");

        connection.replace(Arc::new(Node("second")));

        assert_eq!(answered(held.as_ref()).await, "second");
    }
}

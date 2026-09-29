use std::fmt;

use crate::{Batch, BoxFuture, CallResult, Client, Error, ErrorKind, Repetition, Request};

/// Ordered failover across several inner clients, one per node.
///
/// A [`Repetition::Permitted`] call visits clients in order and advances only
/// after a retryable transport failure; a received error object or a terminal
/// transport failure stops at that client. [`Repetition::Forbidden`] work
/// reaches the first client and no other, so a later node can never hide that
/// an earlier one accepted the envelope.
///
/// `Debug` shows the client count and nothing else.
///
/// The client list is held as a first client and the rest, so "at least one
/// node" is the type's shape rather than a runtime check: every walk below
/// starts at a client that exists, and ends holding a failure it observed.
#[derive(Clone)]
pub struct Failover<C> {
    first: C,
    rest: Vec<C>,
}

impl<C> fmt::Debug for Failover<C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Failover")
            .field("client_count", &(self.rest.len() + 1))
            .finish()
    }
}

impl<C: Client> Failover<C> {
    /// Fails over between `clients` in the order given, one per node.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::InvalidConfiguration`] when `clients` is empty.
    pub fn new(clients: Vec<C>) -> Result<Self, Error> {
        let mut clients = clients.into_iter();
        let Some(first) = clients.next() else {
            return Err(Error::new(
                ErrorKind::InvalidConfiguration,
                "JSON-RPC requires at least one endpoint",
            ));
        };
        Ok(Self {
            first,
            rest: clients.collect(),
        })
    }

    async fn walk_request(&self, request: Request) -> Result<CallResult, Error> {
        if request.repetition() == Repetition::Forbidden {
            return self.first.request(request).await;
        }
        let mut last = match self.first.request(request.clone()).await {
            Err(error) if error.is_retryable() => error,
            outcome => return outcome,
        };
        for client in &self.rest {
            match client.request(request.clone()).await {
                Err(error) if error.is_retryable() => last = error,
                outcome => return outcome,
            }
        }
        Err(last)
    }

    async fn walk_batch(&self, batch: Batch) -> Result<Vec<CallResult>, Error> {
        if batch.repetition() == Repetition::Forbidden {
            return self.first.batch(batch).await;
        }
        let mut last = match self.first.batch(batch.clone()).await {
            Err(error) if error.is_retryable() => error,
            outcome => return outcome,
        };
        for client in &self.rest {
            match client.batch(batch.clone()).await {
                Err(error) if error.is_retryable() => last = error,
                outcome => return outcome,
            }
        }
        Err(last)
    }
}

impl<C: Client> Client for Failover<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        Box::pin(self.walk_request(request))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        Box::pin(self.walk_batch(batch))
    }
}

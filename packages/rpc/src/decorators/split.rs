//! [`Split`]: a batch in requests of at most `rpc.max_batch` calls.

use std::num::NonZeroUsize;

use crate::{Answer, Batch, BoxFuture, Client, Error, RawJson, Request, Url, client::answer};

/// What [`Split`] reads from a node's URL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SplitParams {
    /// `rpc.max_batch`: the most calls one request carries; 1 sends no
    /// batches. `None` when unwritten: no limit.
    pub max_batch: Option<NonZeroUsize>,
}

impl TryFrom<&Url> for SplitParams {
    type Error = Error;

    fn try_from(url: &Url) -> Result<Self, Error> {
        let max_batch = url.params.namespace("rpc").get_int("max_batch")?;
        Ok(Self { max_batch })
    }
}

/// `C`, sending a batch in requests of at most `max` calls, one after
/// another, each under the batch's timeout; answers in call order. The first
/// failing request fails the batch. At most 1 sends each call alone, for a
/// node that takes no batches. `None`: every batch as it is.
#[derive(Clone, Debug)]
pub struct Split<C> {
    inner: C,
    max: Option<NonZeroUsize>,
}

impl<C> Split<C> {
    /// `inner`, sent batches of at most `max` calls.
    #[must_use]
    pub const fn new(inner: C, max: Option<NonZeroUsize>) -> Self {
        Self { inner, max }
    }
}

impl<C: Client> Split<C> {
    async fn send(&self, batch: Batch) -> Result<Vec<Answer>, Error> {
        let Some(max) = self.max else {
            return self.inner.batch(batch).await;
        };
        let mut results = Vec::new();
        if max.get() == 1 {
            for request in batch.requests() {
                results.push(answer(self.inner.request(request).await)?);
            }
            return Ok(results);
        }
        for chunk in batch.chunks(max) {
            results.extend(self.inner.batch(chunk).await?);
        }
        Ok(results)
    }
}

impl<C: Client> Client for Split<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        self.inner.request(request)
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.send(batch))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        self.inner.broadcast(request)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use futures_executor::block_on;
    use serde_json::json;

    use super::Split;
    use crate::{
        Batch, Call, Client, ErrorKind,
        testing::{Fault, Faults, Reply, Script},
    };

    fn split(script: &Script, max: usize) -> (Split<Faults<Script>>, crate::testing::Switch) {
        let faults = Faults::new(script.clone());
        let switch = faults.switch();
        (Split::new(faults, NonZeroUsize::new(max)), switch)
    }

    fn calls(count: usize) -> Vec<Call> {
        (0..count).map(|_| Call::new("m", json!([]))).collect()
    }

    fn replies(count: usize) -> Vec<Reply> {
        (0..count).map(|n| Reply::ok("m", json!(n))).collect()
    }

    #[test]
    fn a_large_batch_goes_in_requests_of_at_most_max_in_order() {
        let script = Script::new(replies(5));
        let (split, switch) = split(&script, 2);
        let results = block_on(split.batch(Batch::new(calls(5)).unwrap())).unwrap();
        let numbers: Vec<u64> = results
            .iter()
            .map(|result| result.as_ref().unwrap().deserialize().unwrap())
            .collect();
        assert_eq!(numbers, [0, 1, 2, 3, 4]);
        assert_eq!(switch.requests(), 3);
    }

    #[test]
    fn max_1_sends_each_call_alone_and_a_batch_within_max_is_one_request() {
        let script = Script::new(replies(3));
        let (alone, switch) = split(&script, 1);
        block_on(alone.batch(Batch::new(calls(3)).unwrap())).unwrap();
        assert_eq!((switch.requests(), switch.calls("m")), (3, 3));

        script.push(replies(3));
        let (within, switch) = split(&script, 10);
        block_on(within.batch(Batch::new(calls(3)).unwrap())).unwrap();
        assert_eq!(switch.requests(), 1);
        script.exhausted();
    }

    #[test]
    fn a_failing_chunk_fails_the_batch() {
        let script = Script::new(replies(2));
        let (split, switch) = split(&script, 2);
        switch.limit(Some(1), Fault::Transport(ErrorKind::HttpStatus(429)));
        let error = block_on(split.batch(Batch::new(calls(4)).unwrap())).unwrap_err();
        assert_eq!(error.kind(), Some(ErrorKind::HttpStatus(429)));
        script.exhausted();
    }

    #[test]
    fn a_chunk_keeps_once_only_and_the_timeout() {
        let script = Script::new(replies(3)).once_only("m");
        let (split, _) = split(&script, 1);
        let batch = Batch::once(calls(3)).unwrap();
        block_on(split.batch(batch.clone())).unwrap();
        let timeout = batch.timeout();
        assert!(
            batch
                .requests()
                .iter()
                .all(|r| r.timeout() == Some(timeout))
        );
        let chunks = Batch::once(calls(3))
            .unwrap()
            .chunks(NonZeroUsize::MIN.saturating_add(1));
        assert!(
            chunks
                .iter()
                .all(|c| c.repetition() == crate::Repetition::Forbidden)
        );
        assert!(chunks.iter().all(|c| c.timeout() == timeout));
    }
}

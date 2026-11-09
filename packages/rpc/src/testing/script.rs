use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};

use serde_json::Value;

use crate::{Batch, BoxFuture, CallResult, Client, Error, Failure, RawJson, Repetition, Request};

/// One expected call and the reply it gets.
pub struct Reply {
    method: &'static str,
    params: Option<Value>,
    result: Result<CallResult, Error>,
}

impl Reply {
    /// Replies with `value` serialized as the call's result.
    ///
    /// # Panics
    ///
    /// When `value` does not serialize.
    #[must_use]
    pub fn ok(method: &'static str, value: Value) -> Self {
        let raw = RawJson::from_serializable(&value).expect("a scripted result must serialize");
        Self::result(method, Ok(Ok(raw)))
    }

    /// Replies with exact bytes, which must be one valid JSON value.
    ///
    /// # Panics
    ///
    /// When `bytes` are not valid JSON.
    #[must_use]
    pub fn raw(method: &'static str, bytes: Vec<u8>) -> Self {
        let raw = RawJson::new(bytes).expect("a scripted result must be valid JSON");
        Self::result(method, Ok(Ok(raw)))
    }

    /// Replies with a server error object: a result, not a transport failure.
    #[must_use]
    pub fn failure(method: &'static str, code: i64, message: &str) -> Self {
        let failure = Failure {
            code,
            message: message.to_owned(),
            data: None,
        };
        Self::result(method, Ok(Err(failure)))
    }

    /// Replies with any outcome, a transport [`Error`] included.
    #[must_use]
    pub fn result(method: &'static str, result: Result<CallResult, Error>) -> Self {
        Self {
            method,
            params: None,
            result,
        }
    }

    /// Also asserts the exact parameters of the call.
    #[must_use]
    pub fn expecting(mut self, params: Value) -> Self {
        self.params = Some(params);
        self
    }
}

#[derive(Default)]
struct State {
    replies: VecDeque<Reply>,
    requests: Vec<(String, Value)>,
}

/// The node double. Clones share one script.
#[derive(Clone, Default)]
pub struct Script {
    state: Arc<Mutex<State>>,
    once_only: Option<&'static str>,
}

impl Script {
    /// A script that replies to the calls it lists, in order.
    #[must_use]
    pub fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        let script = Self::default();
        script.push(replies);
        script
    }

    /// The same script, with `method` pinned to exactly one execution: a
    /// request for it that carries [`Repetition::Permitted`] fails the test.
    /// A chain names its submission method here, which is the assertion that
    /// its submitter never lets a decorator replay an envelope.
    #[must_use]
    pub fn once_only(mut self, method: &'static str) -> Self {
        self.once_only = Some(method);
        self
    }

    /// Appends replies to the end of the script.
    pub fn push(&self, replies: impl IntoIterator<Item = Reply>) {
        self.state().replies.extend(replies);
    }

    /// Every call made so far: method and parameters.
    #[must_use]
    pub fn requests(&self) -> Vec<(String, Value)> {
        self.state().requests.clone()
    }

    /// Every call made so far, by method.
    #[must_use]
    pub fn methods(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|(method, _)| method)
            .collect()
    }

    /// Asserts that every scripted reply was consumed.
    ///
    /// # Panics
    ///
    /// When the script still holds a reply.
    pub fn exhausted(&self) {
        assert!(self.state().replies.is_empty(), "missing RPC");
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("script lock must be healthy")
    }

    fn take(&self, method: &str, params: Value) -> Result<CallResult, Error> {
        let mut state = self.state();
        let reply = state
            .replies
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected RPC {method}"));
        assert_eq!(method, reply.method, "RPC method order");
        if let Some(expected) = &reply.params {
            assert_eq!(&params, expected, "{method} parameters");
        }
        state.requests.push((method.to_owned(), params));
        reply.result
    }

    fn pinned(&self, method: &str, repetition: Repetition) {
        assert!(
            repetition == Repetition::Forbidden || self.once_only != Some(method),
            "{method} must be once-only"
        );
    }
}

impl Client for Script {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<CallResult, Error>> {
        let repetition = request.repetition();
        let call = request.into_call();
        self.pinned(&call.method, repetition);
        let result = self.take(&call.method, call.params);
        Box::pin(async move { result })
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        let repetition = batch.repetition();
        let results = batch
            .into_calls()
            .into_iter()
            .map(|call| {
                self.pinned(&call.method, repetition);
                self.take(&call.method, call.params)
            })
            .collect::<Result<Vec<_>, _>>();
        Box::pin(async move { results })
    }
}

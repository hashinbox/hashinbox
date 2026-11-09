//! [`Faults`]: any client, failing on demand.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use crate::{
    Batch, BoxFuture, Call, CallError, CallResult, Client, Error, ErrorKind, Failure, RawJson,
    Request, client::narrow,
};

/// What a [`Faults`] answers instead of its client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A transport failure of this kind; the client is not asked.
    Transport(ErrorKind),
    /// A JSON-RPC error object (a node's recorded `(code, message)`); the
    /// client is not asked.
    Remote(i64, String),
    /// The client executes the call and its answer is lost: a timeout. The
    /// outcome of a submission is then unknown.
    Lost,
    /// The client's answer with its result replaced by one that decodes as
    /// nothing a caller asked for.
    Broken,
}

/// The failures planned so far, and what was asked.
#[derive(Default)]
struct Plan {
    down: bool,
    delay: Duration,
    /// One-shot faults, each for one method or any.
    next: VecDeque<(Option<String>, Fault)>,
    /// After this many requests, every request fails with the fault.
    limit: Option<(usize, Fault)>,
    /// Every method asked, in order; a batch counts once per call.
    asked: Vec<String>,
    /// Requests (a batch is one) so far.
    requests: usize,
}

/// The controls of a [`Faults`]; clones share them.
#[derive(Clone, Default)]
pub struct Switch(Arc<Mutex<Plan>>);

impl Switch {
    /// Every call fails as an unreachable node while `on`.
    pub fn down(&self, on: bool) {
        self.plan().down = on;
    }

    /// Every call waits `delay` first.
    pub fn delay(&self, delay: Duration) {
        self.plan().delay = delay;
    }

    /// The next call of `method` (any, when `None`) answers `fault`.
    pub fn fail_next(&self, method: Option<&str>, fault: Fault) {
        self.plan()
            .next
            .push_back((method.map(str::to_owned), fault));
    }

    /// After `after` more requests, every request answers `fault` (a rate
    /// limit); `None` lifts it.
    pub fn limit(&self, after: Option<usize>, fault: Fault) {
        let mut plan = self.plan();
        let start = plan.requests;
        plan.limit = after.map(|after| (start + after, fault));
    }

    /// How many calls of `method` reached the switch.
    #[must_use]
    pub fn calls(&self, method: &str) -> usize {
        self.plan().asked.iter().filter(|m| *m == method).count()
    }

    /// How many requests (a batch is one) reached the switch.
    #[must_use]
    pub fn requests(&self) -> usize {
        self.plan().requests
    }

    fn plan(&self) -> MutexGuard<'_, Plan> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Logs one request of `methods`; the delay and what fails the whole
    /// request, if anything.
    fn begin(&self, methods: &[&str]) -> (Duration, Option<Fault>) {
        let mut plan = self.plan();
        plan.asked.extend(methods.iter().map(|m| (*m).to_owned()));
        plan.requests += 1;
        if plan.down {
            return (plan.delay, Some(Fault::Transport(ErrorKind::Unavailable)));
        }
        let limited = plan
            .limit
            .as_ref()
            .filter(|(from, _)| plan.requests > *from)
            .map(|(_, fault)| fault.clone());
        (plan.delay, limited)
    }

    /// The one-shot fault planned for the next call of `method`.
    fn take(&self, method: &str) -> Option<Fault> {
        let mut plan = self.plan();
        let at = plan
            .next
            .iter()
            .position(|(only, _)| only.as_deref().is_none_or(|only| only == method))?;
        plan.next.remove(at).map(|(_, fault)| fault)
    }
}

/// `C`, failing as its [`Switch`] plans: down, slow, rate limited, failing
/// the next call of a method, or answering a broken result.
pub struct Faults<C> {
    client: C,
    switch: Switch,
}

impl<C: Client> Faults<C> {
    /// `client`, failing nothing yet.
    #[must_use]
    pub fn new(client: C) -> Self {
        Self {
            client,
            switch: Switch::default(),
        }
    }

    /// The controls.
    #[must_use]
    pub fn switch(&self) -> Switch {
        self.switch.clone()
    }

    async fn one(&self, call: Call, fault: Option<Fault>, request: Request) -> CallOutcome {
        match fault {
            None => self.client.request(request).await,
            Some(Fault::Lost) => {
                let _ = self.client.request(request).await;
                Err(lost())
            }
            Some(Fault::Broken) => self.client.request(request).await.map(|_| Ok(broken())),
            Some(fault) => answer(&call, fault),
        }
    }
}

type CallOutcome = Result<CallResult, Error>;

/// `fault` as an answer that never reached a node.
fn answer(call: &Call, fault: Fault) -> CallOutcome {
    match fault {
        Fault::Transport(kind) => Err(Error::new(kind, format!("{} failed (fault)", call.method))),
        Fault::Remote(code, message) => Ok(Err(Failure {
            code,
            message,
            data: None,
        })),
        Fault::Lost => Err(lost()),
        Fault::Broken => Ok(Ok(broken())),
    }
}

/// Waits `delay`; no timer at all when it is zero, so a runtime-less
/// caller (`block_on`) works.
async fn pause(delay: Duration) {
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

fn lost() -> Error {
    Error::new(ErrorKind::Timeout, "the answer was lost (fault)")
}

fn broken() -> RawJson {
    RawJson(br#"{"broken":true}"#.to_vec())
}

impl<C: Client> Client for Faults<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, CallOutcome> {
        Box::pin(async move {
            let call = request.clone().into_call();
            let (delay, whole) = self.switch.begin(&[&call.method]);
            pause(delay).await;
            let fault = whole.or_else(|| self.switch.take(&call.method));
            self.one(call, fault, request).await
        })
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<CallResult>, Error>> {
        Box::pin(async move {
            let calls = batch.clone().into_calls();
            let methods: Vec<&str> = calls.iter().map(|call| call.method.as_str()).collect();
            let (delay, whole) = self.switch.begin(&methods);
            pause(delay).await;
            if let Some(fault) = whole {
                return calls
                    .iter()
                    .map(|call| answer(call, fault.clone()))
                    .collect();
            }
            let faults: Vec<_> = calls.iter().map(|c| self.switch.take(&c.method)).collect();
            let lost = faults.contains(&Some(Fault::Lost));
            let mut answers = self.client.batch(batch).await?;
            if lost {
                return Err(self::lost());
            }
            for ((answer, fault), call) in answers.iter_mut().zip(faults).zip(&calls) {
                match fault {
                    None => {}
                    Some(Fault::Broken) => *answer = Ok(broken()),
                    Some(fault) => *answer = self::answer(call, fault)?,
                }
            }
            Ok(answers)
        })
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), CallError>> {
        Box::pin(async move {
            let call = request.clone().into_call();
            let (delay, whole) = self.switch.begin(&[&call.method]);
            pause(delay).await;
            match whole.or_else(|| self.switch.take(&call.method)) {
                None => self.client.broadcast(request).await,
                Some(Fault::Lost) => {
                    let _ = self.client.broadcast(request).await;
                    Err(CallError::Transport(lost()))
                }
                Some(fault) => narrow(answer(&call, fault)),
            }
        })
    }
}

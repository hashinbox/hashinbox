//! [`Logged`]: a node's failure streaks and recovery, to an injected logger.

use logging::{Level, Logger};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    Answer, Batch, BoxFuture, Client, Endpoint, Error, RawJson, Request,
    decorators::counted::Outcome,
};

/// `C`, its failures logged: the first of a streak warns, a repeat warns at
/// most every 30 seconds with a count, and the next success says it
/// recovered. Clones share the streak. Scope `logger` to the node by index;
/// no URL, parameter or provider prose reaches it.
#[derive(Clone)]
pub struct Logged<C> {
    inner: C,
    events: Arc<Events>,
}

impl<C> Logged<C> {
    /// `inner`, logged to `logger`.
    #[must_use]
    pub fn new(inner: C, logger: Logger) -> Self {
        Self {
            inner,
            events: Arc::new(Events::new(logger)),
        }
    }

    async fn log<T: Outcome>(&self, sent: impl Future<Output = T>) -> T {
        let outcome = sent.await;
        let failure = outcome.failure();
        if !failure.as_ref().is_some_and(Error::is_local) {
            self.events.outcome(failure.as_ref().map(Error::summary));
        }
        outcome
    }
}

impl<C: Client> Client for Logged<C> {
    fn request<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<RawJson, Error>> {
        Box::pin(self.log(self.inner.request(request)))
    }

    fn batch<'a>(&'a self, batch: Batch) -> BoxFuture<'a, Result<Vec<Answer>, Error>> {
        Box::pin(self.log(self.inner.batch(batch)))
    }

    fn broadcast<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<(), Error>> {
        Box::pin(self.log(self.inner.broadcast(request)))
    }

    fn endpoint(&self) -> Option<&Endpoint> {
        self.inner.endpoint()
    }
}

/// One node's current failure streak. Different nodes never share a streak.
struct Events {
    logger: Logger,
    failure: Mutex<Option<Streak>>,
}

struct Streak {
    kind: String,
    count: u64,
    reported: Instant,
}

impl Events {
    fn new(logger: Logger) -> Self {
        Self {
            logger,
            failure: Mutex::new(None),
        }
    }

    fn outcome(&self, failure: Option<String>) {
        let notice = self.changed(failure, Instant::now());
        let Some((event, kind, count)) = notice else {
            return;
        };
        let (level, message) = if event == "rpc.recovered" {
            (Level::Info, "RPC node recovered")
        } else {
            (Level::Warn, "RPC node request failed")
        };
        self.logger.event(
            level,
            event,
            message,
            &[("kind", &kind), ("count", &count.to_string())],
        );
    }

    fn changed(
        &self,
        failure: Option<String>,
        now: Instant,
    ) -> Option<(&'static str, String, u64)> {
        let mut held = self
            .failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(kind) = failure else {
            return held
                .take()
                .map(|streak| ("rpc.recovered", streak.kind, streak.count));
        };
        if let Some(streak) = held.as_mut().filter(|streak| streak.kind == kind) {
            streak.count = streak.count.saturating_add(1);
            if now.duration_since(streak.reported) < Duration::from_secs(30) {
                return None;
            }
            streak.reported = now;
            return Some(("rpc.failed", kind, streak.count));
        }
        *held = Some(Streak {
            kind: kind.clone(),
            count: 1,
            reported: now,
        });
        Some(("rpc.failed", kind, 1))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use futures_executor::block_on;
    use logging::{Filter, Logger, Memory};
    use serde_json::json;

    use super::{Events, Logged};
    use crate::{
        Client, ErrorKind, Request,
        testing::{Fault, Faults, Reply, Script},
    };

    #[test]
    fn a_streak_warns_once_then_recovers() {
        let memory = Arc::new(Memory::new(100, 1 << 20));
        let script = Script::new([Reply::ok("m", json!(1))]);
        let faults = Faults::new(script);
        let switch = faults.switch();
        let logged = Logged::new(faults, Logger::new(memory.clone()).with("node", "0"));
        for _ in 0..2 {
            switch.fail_next(None, Fault::Transport(ErrorKind::Unavailable));
            block_on(logged.request(Request::new("m", json!(["secret"])))).unwrap_err();
        }
        block_on(logged.request(Request::new("m", json!([])))).unwrap();
        let entries = memory.page(0, 10, &Filter::default()).entries;
        let events: Vec<(&str, &str)> = entries
            .iter()
            .map(|entry| (entry.event.as_str(), entry.fields["count"].as_str()))
            .collect();
        assert_eq!(events, [("rpc.failed", "1"), ("rpc.recovered", "2")]);
        for entry in &entries {
            assert_eq!(entry.fields["kind"], "JSON-RPC transport is unavailable");
            assert_eq!(entry.fields["node"], "0");
            assert!(!format!("{:?}", entry.fields).contains("secret"));
        }
    }

    #[test]
    fn a_long_streak_reports_its_count_every_30_seconds() {
        let events = Events::new(Logger::default());
        let start = Instant::now();
        let failed = || Some("down".to_owned());
        assert!(events.changed(failed(), start).is_some());
        assert!(events.changed(failed(), start).is_none());
        let later = start + Duration::from_secs(31);
        assert_eq!(
            events.changed(failed(), later),
            Some(("rpc.failed", "down".to_owned(), 3))
        );
    }
}

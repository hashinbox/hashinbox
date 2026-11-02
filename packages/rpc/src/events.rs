use logging::{Level, Logger};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// One endpoint's current failure streak. Different nodes never share a streak.
#[derive(Default)]
pub(crate) struct Events {
    logger: Logger,
    failure: Mutex<Option<Streak>>,
}

struct Streak {
    kind: String,
    count: u64,
    reported: Instant,
}

impl Events {
    pub(crate) fn new(logger: Logger) -> Self {
        Self {
            logger,
            failure: Mutex::new(None),
        }
    }

    pub(crate) fn outcome(&self, failure: Option<String>) {
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

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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failures_are_grouped_until_summary_or_recovery() {
        let events = Events::default();
        let now = Instant::now();
        assert_eq!(events.changed(Some("timeout".into()), now).unwrap().2, 1);
        assert!(events.changed(Some("timeout".into()), now).is_none());
        assert_eq!(
            events
                .changed(Some("timeout".into()), now + Duration::from_secs(30))
                .unwrap()
                .2,
            3
        );
        assert_eq!(
            events.changed(None, now).unwrap(),
            ("rpc.recovered", "timeout".into(), 3)
        );
        assert!(events.changed(None, now).is_none());
    }
}

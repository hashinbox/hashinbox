//! [`Jobs`]: the queue's rules over many jobs, as [`Job`] holds them for one.

use std::ops::Deref;

use crate::task::{Id, Job, Status};

/// Jobs ordered by id: oldest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Jobs(Vec<Job>);

impl Jobs {
    /// The pending jobs due at `now` that nothing blocks (see
    /// [`Self::blocker`]), oldest first. No two of them conflict.
    #[must_use]
    pub fn startable(&self, now: u64) -> Vec<&Job> {
        self.0
            .iter()
            .enumerate()
            .filter(|(_, job)| job.status == Status::Pending && job.run_at <= now)
            .filter(|(at, _)| self.blocking(*at).is_none())
            .map(|(_, job)| job)
            .collect()
    }

    /// What holds back the job `id`: a running job, or an earlier unfinished
    /// one, of a conflicting topic; `None` when nothing does or `id` is not
    /// here.
    #[must_use]
    pub fn blocker(&self, id: &Id) -> Option<&Job> {
        let at = self.0.iter().position(|job| job.id == *id)?;
        self.blocking(at)
    }

    /// Whether every job is done or failed.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.0.iter().all(|job| job.status.is_finished())
    }

    /// The first job blocking the one at `at`.
    fn blocking(&self, at: usize) -> Option<&Job> {
        let job = &self.0[at];
        self.0
            .iter()
            .enumerate()
            .find(|(other, blocker)| {
                *other != at
                    && !blocker.status.is_finished()
                    && (*other < at || blocker.status == Status::Running)
                    && blocker.topic.conflicts(&job.topic)
            })
            .map(|(_, blocker)| blocker)
    }
}

impl From<Vec<Job>> for Jobs {
    /// `jobs`, put in id order.
    fn from(mut jobs: Vec<Job>) -> Self {
        jobs.sort_by(|a, b| a.id.cmp(&b.id));
        Self(jobs)
    }
}

impl Deref for Jobs {
    type Target = [Job];

    fn deref(&self) -> &[Job] {
        &self.0
    }
}

impl IntoIterator for Jobs {
    type Item = Job;
    type IntoIter = std::vec::IntoIter<Job>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::task::Topic;

    fn job(topic: &str, status: Status) -> Job {
        let job = Job::new(Topic::parse(topic).unwrap(), "k", json!(null));
        Job { status, ..job }
    }

    #[test]
    fn nothing_starts_beside_or_ahead_of_a_conflicting_job() {
        // b w1, t1 (the whole network), b w1, b w2, and a later network.
        let jobs = [
            job("n.w1", Status::Running),
            job("n", Status::Pending),
            job("n.w1", Status::Pending),
            job("n.w2", Status::Pending),
            job("m", Status::Pending).run_at(10),
        ];
        let ids: Vec<Id> = jobs.iter().map(|job| job.id.clone()).collect();
        let jobs = Jobs::from(jobs.into_iter().rev().collect::<Vec<_>>());
        assert!(jobs.startable(5).is_empty());
        let started: Vec<&Id> = jobs.startable(10).into_iter().map(|job| &job.id).collect();
        assert_eq!(started, [&ids[4]]);
        assert_eq!(jobs.blocker(&ids[1]).map(|job| &job.id), Some(&ids[0]));
        assert_eq!(jobs.blocker(&ids[3]).map(|job| &job.id), Some(&ids[1]));
        assert_eq!(jobs.blocker(&ids[0]), None);
        assert!(!jobs.finished());
        let done: Jobs = vec![job("a", Status::Done), job("b", Status::Failed)].into();
        assert!(done.finished());
    }
}

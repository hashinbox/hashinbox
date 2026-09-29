//! Who runs a submission by default: [`Spawner`], used until the application
//! supplies its own `wallets::SubmissionRegistrar`
//! (`WalletFactory::with_registrar`).

use wallets::{RegistrationFuture, SubmissionRegistrar, SubmissionTask};

/// The in-process default registrar: spawns every task on the current Tokio
/// runtime and never refuses one. Nothing waits for its tasks on shutdown.
// linter:disable rust/empty-struct -- Stateless: it spawns on the registering Tokio runtime.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Spawner;

impl SubmissionRegistrar for Spawner {
    fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a> {
        Box::pin(async move {
            tokio::spawn(task.run());
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };

    use wallets::RegistrationError;

    use super::*;

    struct Registrar {
        outcome: Result<(), RegistrationError>,
        inserted: Arc<Mutex<Vec<SubmissionTask>>>,
    }

    impl SubmissionRegistrar for Registrar {
        fn register<'a>(&'a self, task: SubmissionTask) -> RegistrationFuture<'a> {
            Box::pin(async move {
                self.outcome?;
                self.inserted.lock().unwrap().push(task);
                Ok(())
            })
        }
    }

    fn dormant() -> (SubmissionTask, wallets::Activation, Arc<AtomicBool>) {
        let completed = Arc::new(AtomicBool::new(false));
        let task_completed = Arc::clone(&completed);
        let (task, activation) = SubmissionTask::dormant(async move {
            task_completed.store(true, Ordering::Release);
        });
        (task, activation, completed)
    }

    #[tokio::test]
    async fn acknowledges_only_an_inserted_task() {
        let inserted = Arc::new(Mutex::new(Vec::new()));
        let registrar = Registrar {
            outcome: Ok(()),
            inserted: Arc::clone(&inserted),
        };
        let (task, activation, completed) = dormant();
        registrar
            .register(task)
            .await
            .expect("inserted task acknowledgement");

        let task = inserted.lock().unwrap().pop().expect("registered task");
        assert!(!completed.load(Ordering::Acquire));
        activation.start();
        task.run().await;
        assert!(completed.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn closed_and_lost_acknowledgement_do_not_insert_or_run() {
        for outcome in [
            RegistrationError::Closed,
            RegistrationError::AcknowledgementLost,
        ] {
            let inserted = Arc::new(Mutex::new(Vec::new()));
            let registrar = Registrar {
                outcome: Err(outcome),
                inserted: Arc::clone(&inserted),
            };
            let (task, activation, completed) = dormant();
            let error = registrar
                .register(task)
                .await
                .expect_err("registration must fail");

            assert_eq!(error, outcome);
            assert!(inserted.lock().unwrap().is_empty());
            drop(activation);
            assert!(!completed.load(Ordering::Acquire));
        }
    }

    #[tokio::test]
    async fn the_default_spawner_runs_an_activated_task_and_drops_an_unactivated_one() {
        let (task, activation, completed) = dormant();
        Spawner.register(task).await.expect("spawned");
        activation.start();
        for _ in 0..100 {
            if completed.load(Ordering::Acquire) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(completed.load(Ordering::Acquire));

        let (task, activation, completed) = dormant();
        Spawner.register(task).await.expect("spawned");
        drop(activation);
        tokio::task::yield_now().await;
        assert!(!completed.load(Ordering::Acquire));
    }
}

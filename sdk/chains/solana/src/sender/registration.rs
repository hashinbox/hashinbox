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

//! Pages of one address's live outputs, read from one snapshot together with
//! the observations that say whether the page may be trusted at all.

use super::{Repository, conflict, models, release};
use crate::{
    BoxFuture, IndexError, IndexedOutput, Inspection, OutputCursor, OutputPage, OutputRequest,
    OutputStore, WatchProgress, storage::Probe,
};

use models::{Output, Wallet};

impl OutputStore for Repository {
    /// One checkpoint-consistent page of live outputs.
    fn list<'a>(&'a self, request: OutputRequest) -> BoxFuture<'a, Result<OutputPage, IndexError>> {
        Box::pin(async move {
            request.validate()?;
            let network = Self::network(&request.network)?;
            let address = request.address.value.as_str();
            let (mut read, checkpoint) = self.database.snapshot(&network).await?;
            // Refused from this snapshot rather than by whoever reads the page:
            // the observations and the rows are one transaction, so the gate
            // and the rows it guards cannot be read at two different lines.
            let observations = Wallet::find_observed_by_address(&mut read, &network, address)
                .await?
                .into_iter()
                .map(WatchProgress::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            Inspection::new(&observations, checkpoint.as_ref()).spendable()?;
            let after = match &request.after {
                // The live set is not append-only -- a spend removes a row -- so
                // a cursor is still bound to the checkpoint it was issued at.
                Some(cursor) if cursor.checkpoint != checkpoint => {
                    return Err(conflict("outputs changed during pagination"));
                }
                Some(cursor) => Output::resumed(&cursor.position)?,
                None => (String::new(), -1),
            };
            let probe = Probe::new(request.limit);
            let live = Output::find_live(&mut read, &network, address, &after, probe.rows())
                .await?
                .into_iter()
                .map(IndexedOutput::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            release(read).await?;
            let (outputs, next) = probe.page(live, |output| OutputCursor {
                checkpoint: checkpoint.clone(),
                position: Output::position(&output.key()),
                // Nothing here folds a pending block, so there is no revision
                // to name: whoever folds one sets it on the way out.
                revision: None,
            });
            Ok(OutputPage {
                checkpoint,
                outputs,
                next,
            })
        })
    }
}

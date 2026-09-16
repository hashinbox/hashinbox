use std::sync::Arc;

use base::Decimal;

use crate::{Error, ErrorKind, SendError, SendFuture, Sender, Transfer};

use super::{Entry, MAX_TRANSFERS, WalletTransfer, Wallets};

impl<I, F> Wallets<I, F>
where
    I: Clone + Ord + Send + Sync + 'static,
    F: Clone + Ord + Send + Sync + 'static,
{
    pub fn send_all<'a>(&'a self, requests: Vec<WalletTransfer<I>>) -> SendFuture<'a> {
        let prepared = self.transfers(requests);
        Box::pin(async move {
            let (sender, transfers) = prepared?;
            sender.send(transfers).await
        })
    }

    fn transfers(
        &self,
        requests: Vec<WalletTransfer<I>>,
    ) -> Result<(Arc<dyn Sender>, Vec<Transfer>), SendError> {
        if requests.is_empty() {
            return Err(SendError::collection(
                ErrorKind::InvalidBatch,
                "at least one transfer is required",
            ));
        }
        if requests.len() > MAX_TRANSFERS {
            return Err(SendError::collection(
                ErrorKind::InvalidBatch,
                "at most 50 transfers are allowed",
            ));
        }
        let mut family = None;
        let mut transfers = Vec::with_capacity(requests.len());
        for (index, request) in requests.into_iter().enumerate() {
            let entry = self
                .transfer_entry(&request, family.as_ref())
                .map_err(|error| SendError::item(index, Vec::new(), error))?;
            family = Some(entry.info.family.clone());
            transfers.push(Transfer {
                wallet: entry.wallet,
                to: request.to,
                amount: request.amount,
            });
        }
        let family = family.ok_or_else(|| {
            SendError::operation(ErrorKind::Unsupported, "transfer family is missing")
        })?;
        let sender = self
            .families
            .get(&family)
            .map(|configured| configured.sender.clone())
            .ok_or_else(|| {
                SendError::operation(ErrorKind::Unsupported, "wallet family is not configured")
            })?;
        Ok((sender, transfers))
    }

    fn transfer_entry(
        &self,
        request: &WalletTransfer<I>,
        family: Option<&F>,
    ) -> Result<Entry<I, F>, Error> {
        if request.amount <= Decimal::zero() {
            return Err(Error::new(
                ErrorKind::InvalidAmount,
                "amount must be positive",
            ));
        }
        let entry = self.entry(&request.wallet)?;
        if family.is_some_and(|expected| expected != &entry.info.family) {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "all transfers must use the same wallet family",
            ));
        }
        Ok(entry)
    }
}

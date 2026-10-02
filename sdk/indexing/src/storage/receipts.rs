use crate::{BlockRef, Decimal, Fold, IndexError, IndexedBatch, Subject};
use logging::{Level, Logger};
use std::collections::BTreeSet;

/// A newly recorded positive net effect on one watched holding.
pub(crate) struct Receipt {
    pub(crate) subject: Subject,
    pub(crate) block: BlockRef,
    pub(crate) transaction: Option<String>,
    pub(crate) credit: Option<String>,
    amount: Decimal,
}

impl Receipt {
    pub(crate) fn collect(
        batch: &IndexedBatch,
        mut new: impl FnMut(&Self) -> Result<bool, IndexError>,
    ) -> Result<Vec<Self>, IndexError> {
        let mut receipts = Vec::new();
        for block in batch.blocks() {
            for transaction in block.transactions() {
                receipts.extend(Self::transaction(transaction, batch.settlement())?);
            }
            receipts.extend(block.credits().iter().map(|credit| Self {
                subject: Subject {
                    address: credit.to.clone(),
                    asset: credit.asset.clone(),
                },
                block: block.block().clone(),
                amount: credit.amount.clone(),
                transaction: None,
                credit: Some(credit.id.0.clone()),
            }));
        }
        let mut selected = Vec::new();
        for receipt in receipts {
            if receipt.amount != Decimal::zero() && new(&receipt)? {
                selected.push(receipt);
            }
        }
        Ok(selected)
    }

    fn transaction(
        transaction: &crate::CanonicalTransaction,
        settlement: crate::FeeSettlement,
    ) -> Result<Vec<Self>, IndexError> {
        let subjects: BTreeSet<_> = transaction
            .transaction
            .movements
            .iter()
            .filter_map(|movement| {
                movement
                    .to()
                    .map(|address| (address.clone(), movement.asset.clone()))
            })
            .collect();
        subjects
            .into_iter()
            .map(|(address, asset)| {
                let subject = Subject { address, asset };
                let mut fold = Fold::new(&subject);
                fold.apply(transaction, settlement)?;
                let amount = fold.balance(None).amount;
                Ok(Self {
                    subject,
                    block: transaction.block().clone(),
                    amount,
                    transaction: Some(transaction.transaction.transaction_id.id.to_string()),
                    credit: None,
                })
            })
            .collect()
    }

    pub(crate) fn emit(&self, logger: &Logger) {
        let network = &self.subject.address.network;
        let mut scoped = logger
            .with("chain", network.chain.to_string())
            .with("network", network.name.clone())
            .with("component", "wallet")
            .with("address", self.subject.address.value.clone())
            .with("asset", self.subject.asset.asset.clone())
            .with("amount", self.amount.to_string())
            .with("block_position", self.block.position.0.to_string());
        if let Some(id) = &self.transaction {
            scoped = scoped.with("transaction_id", id);
        }
        if let Some(id) = &self.credit {
            scoped = scoped.with("credit_id", id);
        }
        if let Some(time) = self.block.timestamp {
            scoped = scoped.with("block_time", time.to_string());
        }
        scoped.event(
            Level::Info,
            "wallet.received",
            "Confirmed funds indexed",
            &[],
        );
    }
}

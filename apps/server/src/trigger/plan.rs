//! [`Plan`]: the transfers a trigger's `Send` makes over `This`, decided with
//! the evaluation's figures.

use anyhow::{bail, ensure};
use base::{Address, Decimal};

use super::Evaluation;
use crate::models::{
    trigger::{Send, Target},
    wallet_index_balance::WalletIndexBalances,
};

/// One transfer a run makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Planned {
    pub(crate) from: Address,
    pub(crate) to: Address,
    /// Atomic units of the send's asset; above zero.
    pub(crate) amount: Decimal,
}

/// What a `Send` moves now: its asset and its transfers, in source then
/// destination order. Empty when every amount came to zero or less.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) asset: String,
    pub(crate) transfers: Vec<Planned>,
}

impl Evaluation<'_> {
    /// The transfers `send` makes with `this` in context.
    ///
    /// Many sources pay one destination, `amount` evaluated per source; one
    /// source pays many destinations, `amount` per destination; one to one is
    /// per destination when the destination is `This`. A transfer
    /// whose amount is zero, negative or unknown is skipped.
    ///
    /// # Errors
    ///
    /// A read error; many sources and many destinations; a source in
    /// another asset than the send's; a target that is not fully indexed.
    pub(crate) fn plan(&mut self, send: &Send, this: &WalletIndexBalances) -> anyhow::Result<Plan> {
        let Some(sources) = self.target(&send.source, this)? else {
            bail!("a source is not fully indexed");
        };
        let Some(destinations) = self.target(&send.destination, this)? else {
            bail!("a destination is not fully indexed");
        };
        ensure!(
            sources.values().all(|one| one.index.asset.id == send.asset),
            "a source is not in the sent asset"
        );
        let mut transfers = Vec::new();
        ensure!(
            sources.len() <= 1 || destinations.len() <= 1,
            "a send has many sources and many destinations"
        );
        // The side that is many is walked, the other is its one address; one
        // to one is walked on the side the send names as `This`.
        let per_source = match (sources.len(), destinations.len()) {
            (1, 1) => !matches!(send.destination, Target::This(_)),
            (_, destinations) => destinations <= 1,
        };
        let (each, other) = if per_source {
            (&sources, &destinations)
        } else {
            (&destinations, &sources)
        };
        if let Some(other) = other.values().next() {
            let other = &other.index.address;
            for one in each.values() {
                let alone = each.only(one)?;
                if let Some(amount) = self.positive(send, &alone)? {
                    let mine = one.index.address.clone();
                    let (from, to) = if per_source {
                        (mine, other.clone())
                    } else {
                        (other.clone(), mine)
                    };
                    transfers.push(Planned { from, to, amount });
                }
            }
        }
        Ok(Plan {
            asset: send.asset.clone(),
            transfers,
        })
    }

    /// `send`'s amount with `this` in context, when above zero.
    fn positive(
        &mut self,
        send: &Send,
        this: &WalletIndexBalances,
    ) -> anyhow::Result<Option<Decimal>> {
        let amount = self.amount(&send.amount, this, &send.asset)?;
        Ok(amount.filter(|amount| *amount > Decimal::zero()))
    }
}

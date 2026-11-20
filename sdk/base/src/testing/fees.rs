//! [`Fees`]: a fee estimator at fixed prices that keeps what it was asked.

use std::sync::Mutex;

use crate::{Decimal, FeeEstimator, Future, Network, Networked, Spends};

/// Prices each spend at a fixed native fee: `to_new` for a spend to a new
/// address (no recipient named), `to_known` for one to a named recipient.
/// It keeps every set of spends it was asked to price, so a test can tell
/// what was priced, for whom, and that nothing was asked at all.
#[derive(Debug)]
pub struct Fees {
    network: Network,
    to_new: Decimal,
    to_known: Decimal,
    failure: Option<String>,
    asked: Mutex<Vec<Spends>>,
}

impl Fees {
    /// `per_spend` native atomic units for every spend, on `network`.
    #[must_use]
    pub fn new(network: Network, per_spend: Decimal) -> Self {
        Self {
            network,
            to_known: per_spend.clone(),
            to_new: per_spend,
            failure: None,
            asked: Mutex::new(Vec::new()),
        }
    }

    /// The same, a spend to a named recipient priced `price` instead.
    #[must_use]
    pub fn known_recipient(mut self, price: Decimal) -> Self {
        self.to_known = price;
        self
    }

    /// The same, every estimate failing with `why` (as a node that cannot
    /// be reached); what it was asked is still kept.
    #[must_use]
    pub fn failing(mut self, why: impl Into<String>) -> Self {
        self.failure = Some(why.into());
        self
    }

    /// What it was asked to price, in order.
    ///
    /// # Panics
    ///
    /// When a thread panicked while pricing.
    #[must_use]
    pub fn asked(&self) -> Vec<Spends> {
        self.asked.lock().expect("not poisoned").clone()
    }

    /// Keeps `spends` and prices them.
    fn price(&self, spends: &Spends) -> anyhow::Result<Decimal> {
        let Ok(mut asked) = self.asked.lock() else {
            anyhow::bail!("a fee estimate panicked");
        };
        asked.push(spends.clone());
        if let Some(why) = &self.failure {
            anyhow::bail!("{why}");
        }
        let price = |named: bool| if named { &self.to_known } else { &self.to_new };
        Ok(spends.iter().map(|one| price(one.to.is_some())).sum())
    }
}

impl Networked for Fees {
    fn network(&self) -> &Network {
        &self.network
    }
}

impl FeeEstimator for Fees {
    fn estimate<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Decimal>> {
        Box::pin(std::future::ready(self.price(spends)))
    }
}

//! [`Cached`]: Solana's `base::FeeEstimator` that reuses the precise
//! [`Estimator`]'s answers, by Solana's rules, so the node is asked once per
//! time to live, however many wallets send.

use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use base::{Cache, Decimal, Holdings, Spends, TransactionError};

use crate::fee::{Estimator, Prices};

/// Solana's cached `base::FeeEstimator`, which every builder of the factory
/// signs with too (its blockhash and price), so an estimate and the
/// transaction signed after it agree. On Solana a send costs its signatures
/// at one price for every send, plus, for a token, the rent of the
/// recipient's token account when it has none. So it keeps the blockhash and
/// the price of a signature for `ttl`, and the rent for good (it changes only
/// with the network's rent rules); and it takes a recipient that holds the
/// token (as `holdings` say) to have its account, without asking.
pub(crate) struct Cached {
    pub(crate) inner: Estimator,
    /// Who holds what: which token recipients have their account.
    holdings: Arc<dyn Holdings>,
    prices: Cache<Prices>,
    rent: Cache<u64>,
}

impl Cached {
    /// Over `inner`, its prices kept `ttl`, token recipients' accounts as
    /// `holdings` say.
    pub(crate) fn new(inner: Estimator, holdings: Arc<dyn Holdings>, ttl: Duration) -> Self {
        Self {
            inner,
            holdings,
            prices: Cache::new(ttl),
            rent: Cache::new(Duration::MAX),
        }
    }

    /// A recent blockhash and the price of a signature: kept, else asked.
    ///
    /// # Errors
    ///
    /// The precise estimator's failure.
    pub(crate) async fn prices(&self) -> Result<Prices, TransactionError> {
        self.prices.get(|| self.inner.prices()).await
    }

    /// The rent of a token account: kept, else asked.
    async fn rent(&self) -> anyhow::Result<u64> {
        self.rent.get(|| self.inner.rent()).await
    }
}

impl base::Networked for Cached {
    fn network(&self) -> &base::Network {
        &self.inner.network
    }
}

impl base::FeeEstimator for Cached {
    /// Σ per spend one signature's fee, plus, for a token, the rent of the
    /// recipient's token account unless it holds the token (it has its
    /// account then); no recipient: a new address, which has none.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let held = self.holdings.holds(spends).await?;
            let per_signature = self.prices().await?.per_signature;
            let mut total: u64 = 0;
            for (spend, held) in spends.iter().zip(held) {
                let mut fee = per_signature;
                if self.inner.token(spend)? && !held {
                    fee = fee
                        .checked_add(self.rent().await?)
                        .context("Solana fee estimate overflows")?;
                }
                total = total
                    .checked_add(fee)
                    .context("Solana fee estimate overflows")?;
            }
            Ok(Decimal::from(total))
        })
    }
}

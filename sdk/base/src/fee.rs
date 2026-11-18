//! [`Fee`]: the rule a builder signs with, over a chain's [`Rate`] from its
//! [`RateProvider`]; [`Estimator`]: what sending costs on one network, priced
//! from cached network state; [`Holdings`]: what the application knows of who
//! holds what, so a send to an address that already holds its asset is priced
//! as such.

use std::{sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{Address, Asset, Cache, Decimal, Future, Network, Networked, TransactionError};

/// How fast a payment should land.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// Cheaper, and slower.
    Low,
    /// The network's rate now.
    #[default]
    Normal,
    /// Dearer, and faster.
    High,
}

/// One unit's price now, in the chain's smallest coin per unit (Bitcoin's
/// satoshis per 1,000 virtual bytes, Ethereum's wei per gas, Solana's
/// micro-lamports per compute unit). One unit costs `base + tip`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rate {
    /// What everyone must pay now, set by the network's load: Ethereum's
    /// base fee, Bitcoin's mempool minimum, Solana's 0.
    pub base: u128,
    /// What is added on top for speed: Ethereum's tip, Bitcoin's rate above
    /// the mempool minimum, Solana's priority price.
    pub tip: u128,
}

impl Rate {
    /// Both parts × `factor` (to a basis point), rounded up; above one, each
    /// nonzero part at least one unit more. `times(1.1)` meets every chain's
    /// replacement rule.
    #[must_use]
    pub fn times(self, factor: f64) -> Self {
        Self {
            base: times(self.base, factor),
            tip: times(self.tip, factor),
        }
    }

    /// Each part the higher of the two.
    #[must_use]
    pub fn higher(self, other: Self) -> Self {
        Self {
            base: self.base.max(other.base),
            tip: self.tip.max(other.tip),
        }
    }

    /// Only the tip × 0.5, 1 or 2 (`Low`, `Normal`, `High`): the base is
    /// the network's, so `Low` never goes below what nodes accept.
    #[must_use]
    pub fn at(self, level: Level) -> Self {
        let factor = match level {
            Level::Low => 0.5,
            Level::Normal => 1.0,
            Level::High => 2.0,
        };
        Self {
            tip: times(self.tip, factor),
            ..self
        }
    }

    /// What one unit costs: `base + tip`.
    #[must_use]
    pub const fn total(self) -> u128 {
        self.base.saturating_add(self.tip)
    }
}

/// `units` × `factor` (to a basis point), rounded up; above one, at least
/// one unit more for nonzero `units`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "a factor is a small positive number of basis points"
)]
fn times(units: u128, factor: f64) -> u128 {
    let points = (factor * 10_000.0).round().max(0.0) as u128;
    let scaled = units.saturating_mul(points).div_ceil(10_000);
    if points > 10_000 && units > 0 {
        return scaled.max(units.saturating_add(1));
    }
    scaled
}

/// Picks the rate a builder signs at, when it signs: from the network's
/// rate `now` and, when continuing a transaction, the rate it paid, `old`.
pub trait Rule: Send + Sync {
    /// The rate to sign at.
    fn rate(&self, now: Rate, old: Option<Rate>) -> Rate;
}

impl<F> Rule for F
where
    F: Fn(Rate, Option<Rate>) -> Rate + Send + Sync,
{
    fn rate(&self, now: Rate, old: Option<Rate>) -> Rate {
        self(now, old)
    }
}

/// The fee a builder signs with: a common rule by name, or any other.
pub enum Fee {
    /// The network's rate now, at the level; never below the old rate.
    Set(Level),
    /// × factor over the higher of the rate now and the old one:
    /// `Times(1.1)` is the bump.
    Times(f64),
    /// Any other rule.
    Rule(Box<dyn Rule>),
}

impl Fee {
    /// A fee of `rule`.
    pub fn custom(rule: impl Fn(Rate, Option<Rate>) -> Rate + Send + Sync + 'static) -> Self {
        Self::Rule(Box::new(rule))
    }
}

impl Default for Fee {
    /// `Set(Normal)`.
    fn default() -> Self {
        Self::Set(Level::Normal)
    }
}

impl Rule for Fee {
    fn rate(&self, now: Rate, old: Option<Rate>) -> Rate {
        match self {
            Self::Set(level) => {
                let rate = now.at(*level);
                old.map_or(rate, |old| rate.higher(old))
            }
            Self::Times(factor) => old.map_or(now, |old| old.higher(now)).times(*factor),
            Self::Rule(rule) => rule.rate(now, old),
        }
    }
}

/// The network's rate now. One per chain; its factory shares one,
/// [`RateProviderCached`], with every builder and its estimator.
pub trait RateProvider: Networked + Send + Sync {
    /// The rate now.
    ///
    /// # Errors
    ///
    /// The node's failure.
    fn rate(&self) -> Future<'_, Result<Rate, TransactionError>>;
}

/// Any [`RateProvider`], its rate kept for a time to live, so the node is
/// asked once per ttl however many send.
pub struct RateProviderCached {
    inner: Arc<dyn RateProvider>,
    kept: Cache<Rate>,
}

impl RateProviderCached {
    /// Over `inner`, its rate kept `ttl`.
    #[must_use]
    pub const fn new(inner: Arc<dyn RateProvider>, ttl: Duration) -> Self {
        Self {
            inner,
            kept: Cache::new(ttl),
        }
    }
}

impl Networked for RateProviderCached {
    fn network(&self) -> &Network {
        self.inner.network()
    }
}

impl RateProvider for RateProviderCached {
    /// Kept, else asked.
    fn rate(&self) -> Future<'_, Result<Rate, TransactionError>> {
        Box::pin(self.kept.get(|| self.inner.rate()))
    }
}

/// Prices sends on one network from cached network state, so estimating costs
/// arithmetic plus, at most, one store read; RPC only on refresh.
///
/// A chain's wallet factory hands the same estimator to every builder it
/// makes, and builders price their transactions only through it, so an
/// estimate and the fee a builder pays agree.
pub trait Estimator: Networked + Send + Sync {
    /// Worst-case native fee of `spends`, each everything its sender holds of
    /// its asset sent to its recipient: one transaction per sender on account
    /// chains, one transaction for all senders (same asset) on UTXO chains. A
    /// recipient that [`Holdings`] say holds the asset already is priced as
    /// such (less where a first receipt creates an account or balance); one
    /// that does not, or none given, as a new address. A send funded by this
    /// amount goes through; it may overpay slightly, never underpay.
    ///
    /// # Errors
    ///
    /// An asset the network does not send, an address of another network,
    /// or a node or store failure while refreshing.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Decimal>>;
}

/// One send an [`Estimator`] prices: who sends, which asset, and to whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spend {
    /// The sender, who pays the fee.
    pub from: Address,
    /// What it sends.
    pub asset: Asset,
    /// The recipient; `None`: a new address, the dearest case.
    pub to: Option<Address>,
}

/// `(from, asset)`: to a new address.
impl From<(Address, Asset)> for Spend {
    fn from((from, asset): (Address, Asset)) -> Self {
        Self {
            from,
            asset,
            to: None,
        }
    }
}

/// `(from, asset, to)`.
impl From<(Address, Asset, Address)> for Spend {
    fn from((from, asset, to): (Address, Asset, Address)) -> Self {
        Self {
            from,
            asset,
            to: Some(to),
        }
    }
}

/// Spends priced together: one estimate, one [`Holdings`] read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spends(Vec<Spend>);

impl Spends {
    /// Adds `spend`: `(from, asset)` or `(from, asset, to)`.
    pub fn push(&mut self, spend: impl Into<Spend>) {
        self.0.push(spend.into());
    }

    /// Each spend's recipient with its asset, in order; spends to a new
    /// address are left out.
    pub fn recipients(&self) -> impl Iterator<Item = (&Address, &Asset)> {
        self.0
            .iter()
            .filter_map(|spend| spend.to.as_ref().map(|to| (to, &spend.asset)))
    }
}

impl std::ops::Deref for Spends {
    type Target = [Spend];

    fn deref(&self) -> &[Spend] {
        &self.0
    }
}

impl<S: Into<Spend>> FromIterator<S> for Spends {
    fn from_iter<I: IntoIterator<Item = S>>(spends: I) -> Self {
        Self(spends.into_iter().map(Into::into).collect())
    }
}

/// What the application knows of who holds what, from its own records: a
/// send to an address that holds its asset already creates nothing, which
/// costs less on chains where a first receipt creates the recipient's account
/// (Ethereum, Tron) or balance (an ERC-20's, a Solana token account). Read
/// once per estimate or signing.
pub trait Holdings: Send + Sync {
    /// For each of `spends`, in order, whether its recipient, a wallet whose
    /// key is kept, holds its asset now; `false` without a recipient or when
    /// it is not known to. One read for them all.
    ///
    /// # Errors
    ///
    /// A store failure.
    fn holds<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Vec<bool>>>;
}

/// [`Holdings`] that know of no holder: every send is priced as to a new
/// address.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoHoldings;

impl Holdings for NoHoldings {
    fn holds<'a>(&'a self, spends: &'a Spends) -> Future<'a, anyhow::Result<Vec<bool>>> {
        Box::pin(async { Ok(vec![false; spends.len()]) })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use futures_executor::block_on;

    use super::{Fee, Level, Rate, RateProvider, RateProviderCached, Rule as _};
    use crate::{Future, Network, Networked, TransactionError};

    const fn rate(base: u128, tip: u128) -> Rate {
        Rate { base, tip }
    }

    #[test]
    fn a_level_is_its_name_in_snake_case_and_normal_by_default() {
        let levels = [
            (Level::Low, "\"low\""),
            (Level::Normal, "\"normal\""),
            (Level::High, "\"high\""),
        ];
        for (level, text) in levels {
            assert_eq!(serde_json::to_string(&level).unwrap(), text);
            assert_eq!(serde_json::from_str::<Level>(text).unwrap(), level);
        }
        assert_eq!(Level::default(), Level::Normal);
    }

    #[test]
    fn times_rounds_up_and_moves_each_nonzero_part_at_least_one_unit() {
        assert_eq!(rate(100, 10).times(1.1), rate(110, 11));
        assert_eq!(rate(1, 1).times(1.1), rate(2, 2));
        assert_eq!(rate(0, 3).times(1.1), rate(0, 4));
        assert_eq!(rate(15, 3).times(0.5), rate(8, 2));
        assert_eq!(rate(7, 0).times(1.0), rate(7, 0));
    }

    #[test]
    fn a_level_scales_only_the_tip() {
        assert_eq!(rate(100, 10).at(Level::Low), rate(100, 5));
        assert_eq!(rate(100, 10).at(Level::Normal), rate(100, 10));
        assert_eq!(rate(100, 10).at(Level::High), rate(100, 20));
        assert_eq!(rate(100, 10).total(), 110);
    }

    #[test]
    fn higher_takes_each_part_on_its_own() {
        assert_eq!(rate(100, 1).higher(rate(50, 9)), rate(100, 9));
    }

    #[test]
    fn set_pays_the_level_of_the_rate_now_never_below_the_old_one() {
        let now = rate(100, 10);
        assert_eq!(Fee::Set(Level::Low).rate(now, None), rate(100, 5));
        assert_eq!(Fee::default().rate(now, None), now);
        assert_eq!(Fee::Set(Level::High).rate(now, None), rate(100, 20));
        let old = Some(rate(80, 8));
        assert_eq!(Fee::Set(Level::Low).rate(now, old), rate(100, 8));
        assert_eq!(Fee::Set(Level::High).rate(now, old), rate(100, 20));
    }

    #[test]
    fn times_scales_the_higher_of_the_rate_now_and_the_old_one() {
        let now = rate(100, 10);
        assert_eq!(Fee::Times(1.1).rate(now, None), rate(110, 11));
        assert_eq!(Fee::Times(1.1).rate(now, Some(rate(200, 5))), rate(220, 11));
        assert_eq!(Fee::Times(1.1).rate(rate(0, 0), None), rate(0, 0));
        let custom = Fee::custom(|now, _| now.times(3.0));
        assert_eq!(custom.rate(rate(7, 1), Some(rate(100, 100))), rate(21, 3));
    }

    /// Counts its calls; its rate's tip is the count.
    struct Counting(Network, AtomicU64);

    impl Networked for Counting {
        fn network(&self) -> &Network {
            &self.0
        }
    }

    impl RateProvider for Counting {
        fn rate(&self) -> Future<'_, Result<Rate, TransactionError>> {
            let count = self.1.fetch_add(1, Ordering::Relaxed) + 1;
            Box::pin(async move { Ok(rate(0, count.into())) })
        }
    }

    #[test]
    fn a_cached_provider_keeps_its_rate_for_its_time_to_live() {
        let network = Network::new(crate::Chain::from_static("test"), "local");
        let inner = Arc::new(Counting(network, AtomicU64::new(0)));
        let kept = RateProviderCached::new(inner.clone(), Duration::from_secs(60));
        let tip = |provider: &RateProviderCached| block_on(provider.rate()).unwrap().tip;
        assert_eq!(tip(&kept), 1);
        assert_eq!(tip(&kept), 1);
        let fresh = RateProviderCached::new(inner, Duration::ZERO);
        assert_eq!(tip(&fresh), 2);
        assert_eq!(tip(&fresh), 3);
    }
}

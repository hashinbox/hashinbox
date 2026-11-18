//! [`Estimator`]: Bitcoin's `base::FeeEstimator`, the fee of spending
//! every coin the senders hold at the rate now.

use std::sync::Arc;

use anyhow::ensure;
use base::{Asset, Decimal, FeeRateProvider, InputStore};
use bitcoin::Script;

use crate::{FeeRate, fee::vsize, rpc::Client};

/// The largest standard output script: segwit v1 (P2TR) and P2WSH, 34 bytes.
/// An estimate prices its one output as this, since the recipient is unknown.
const WIDEST_OUTPUT: [u8; 34] = [0x51; 34];

/// Bitcoin's `base::FeeEstimator`: the rate of the factory's provider,
/// never below the floor, times the size of spending every coin the senders hold.
pub(crate) struct Estimator {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    pub(crate) rates: Arc<dyn FeeRateProvider>,
    pub(crate) floor: FeeRate,
}

impl base::Networked for Estimator {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl base::FeeEstimator for Estimator {
    /// The fee of `spends` at the rate now: every unspent coin of every
    /// sender in one transaction with one output of the widest standard
    /// script and no change, what a builder pays sending a balance less this
    /// fee (a leftover below dust is fee too). The recipient does not change
    /// it: an output is as wide for any, and the widest is counted.
    ///
    /// # Errors
    ///
    /// An asset other than BTC, or a store or node failure.
    fn estimate<'a>(
        &'a self,
        spends: &'a base::Spends,
    ) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let native = Asset::native(self.network.clone());
            ensure!(
                spends.iter().all(|spend| spend.asset == native),
                "a Bitcoin estimate prices BTC only"
            );
            let addresses: Vec<base::Address> =
                spends.iter().map(|spend| spend.from.clone()).collect();
            let coins = self.inputs.find_unspent(&addresses).await?;
            if coins.is_empty() {
                return Ok(Decimal::from(0_u64));
            }
            let spent = coins
                .iter()
                .map(|coin| {
                    crate::Address::parse(&coin.address.value, self.rpc.network())
                        .map(|address| address.script_pubkey())
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let output = Script::from_bytes(&WIDEST_OUTPUT);
            let rate = FeeRate::from(self.rates.rate().await?).max(self.floor);
            Ok(Decimal::from(
                rate.fee(vsize(spent.iter().map(AsRef::as_ref), [output])),
            ))
        })
    }
}

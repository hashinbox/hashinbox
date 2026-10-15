use std::sync::Arc;

use anyhow::ensure;
use base::{Asset, Cache, Decimal, InputStore, TransactionError};
use bitcoin::Script;

use crate::rpc::Client;

/// Weight of an input without its witness: outpoint, empty script, sequence.
const INPUT_WEIGHT: u64 = 4 * (32 + 4 + 1 + 4);
/// Version, lock time, one-byte counts, segwit marker and flag.
const BASE_WEIGHT: u64 = 4 * (4 + 4 + 1 + 1) + 2;
/// Witness weight spending P2WPKH (signature and key) and P2TR key path.
const P2WPKH_WITNESS: u64 = 109;
const P2TR_WITNESS: u64 = 67;

/// A fee rate in satoshis per 1,000 virtual bytes, Bitcoin Core's unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FeeRate(u64);

impl FeeRate {
    /// Bitcoin Core's default `maxfeerate`: 0.1 BTC/kvB.
    pub const DEFAULT_MAXIMUM: Self = Self(10_000_000);
    /// The highest `maxfeerate` Bitcoin Core accepts: 1 BTC/kvB.
    pub const CORE_LIMIT: Self = Self(100_000_000);

    /// `satoshis` per 1,000 virtual bytes.
    #[must_use]
    pub const fn from_sat_per_kvb(satoshis: u64) -> Self {
        Self(satoshis)
    }

    /// Satoshis per 1,000 virtual bytes.
    #[must_use]
    pub const fn sat_per_kvb(self) -> u64 {
        self.0
    }

    /// The fee for `virtual_size` virtual bytes, rounded up.
    #[must_use]
    pub const fn fee(self, virtual_size: u64) -> u64 {
        (self.0 * virtual_size).div_ceil(1_000)
    }
}

/// Virtual size of a transaction spending outputs that pay `inputs` (each
/// P2WPKH or P2TR) and paying `outputs`.
pub(crate) fn vsize<'a>(
    inputs: impl IntoIterator<Item = &'a Script>,
    outputs: impl IntoIterator<Item = &'a Script>,
) -> u64 {
    let weight = BASE_WEIGHT
        + inputs.into_iter().map(input_weight).sum::<u64>()
        + outputs.into_iter().map(output_weight).sum::<u64>();
    weight.div_ceil(4)
}

/// Weight of an input spending `script`, its witness included.
fn input_weight(script: &Script) -> u64 {
    INPUT_WEIGHT
        + if script.is_p2tr() {
            P2TR_WITNESS
        } else {
            P2WPKH_WITNESS
        }
}

/// Weight of an output paying `script`: value, length prefix, script.
fn output_weight(script: &Script) -> u64 {
    let length = script.len() as u64;
    let prefix = if length < 0xfd { 1 } else { 3 };
    (8 + prefix + length) * 4
}

/// The largest standard output script: segwit v1 (P2TR) and P2WSH, 34 bytes.
/// An estimate prices its one output as this, since the recipient is unknown.
const WIDEST_OUTPUT: [u8; 34] = [0x51; 34];

/// Bitcoin's `base::FeeEstimator`: the node's conservative fee rate, never
/// below the floor, cached for its time to live; every builder of the
/// factory prices with it.
pub(crate) struct Fees {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    pub(crate) target_blocks: u16,
    pub(crate) floor: FeeRate,
    pub(crate) rate: Cache<FeeRate>,
}

impl Fees {
    /// The cached rate, refreshed from `estimatesmartfee` when stale.
    pub(crate) async fn rate(&self) -> Result<FeeRate, TransactionError> {
        self.rate
            .get(|| async {
                let estimate = self
                    .rpc
                    .fee_rate(self.target_blocks)
                    .await
                    .map_err(TransactionError::unavailable)?;
                Ok(estimate.map_or(self.floor, |rate| rate.max(self.floor)))
            })
            .await
    }
}

impl base::Networked for Fees {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl base::FeeEstimator for Fees {
    /// Every unspent coin of every source in one transaction with one output
    /// of the widest standard script and no change: what a builder pays
    /// sending a balance less this fee (a leftover below dust is fee too).
    fn estimate<'a>(
        &'a self,
        sources: &'a [(base::Address, Asset)],
    ) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let native = Asset::native(self.network.clone());
            ensure!(
                sources.iter().all(|(_, asset)| *asset == native),
                "a Bitcoin estimate prices BTC only"
            );
            let addresses: Vec<base::Address> =
                sources.iter().map(|(address, _)| address.clone()).collect();
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
            let rate = self.rate().await?;
            let output = Script::from_bytes(&WIDEST_OUTPUT);
            Ok(Decimal::from(
                rate.fee(vsize(spent.iter().map(AsRef::as_ref), [output])),
            ))
        })
    }
}

//! [`Estimator`]: Solana's `base::FeeEstimator`, asking the node and
//! keeping its answers, by Solana's rules, so the node is asked once per
//! time to live, however many wallets send.

use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, ensure};
use base::{
    Addressed as _, Cache, Decimal, FeeRateProvider, Holdings, Spends, TransactionError,
    TransactionErrorKind,
};
use solana_hash::Hash;
use solana_message::Message;

use crate::{
    Address, Client, Tokens,
    fee::{FLOOR, Priority, fee, limit},
};

/// Bytes of an SPL Token account: what a recipient's new account holds.
const TOKEN_ACCOUNT_BYTES: u64 = 165;

/// The fee payer of the message that prices one signature; never signs.
const PLACEHOLDER: solana_address::Address = solana_address::Address::new_from_array([1; 32]);

/// Solana's `base::FeeEstimator`, which every builder of the factory signs
/// with too (its signature price and rate), so an estimate and the
/// transaction signed after it agree. It asks the node and keeps its
/// answers for `ttl`. On Solana a send costs its signatures at one price
/// for every send, its priority fee at the network's rate (of `rates`),
/// plus, for a token, the rent of the recipient's token account when it has
/// none. So it keeps the price of a signature for `ttl` and the rent for
/// good (it changes only with the network's rent rules); and it takes a
/// recipient that holds the token (as `holdings` say) to have its account,
/// without asking.
pub(crate) struct Estimator {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) tokens: Tokens,
    /// The most a transaction pays per compute unit, in micro-lamports.
    pub(crate) max_priority_fee: u64,
    pub(crate) rates: Arc<dyn FeeRateProvider>,
    /// Who holds what: which token recipients have their account.
    holdings: Arc<dyn Holdings>,
    signature: Cache<u64>,
    rent: Cache<u64>,
}

impl Estimator {
    /// Over `rpc`, sending SOL and `tokens` at the rate of `rates`, paying
    /// at most `max_priority_fee` micro-lamports per compute unit, its
    /// signature price kept `ttl`, token recipients' accounts as `holdings`
    /// say.
    pub(crate) fn new(
        rpc: Client,
        tokens: Tokens,
        max_priority_fee: u64,
        rates: Arc<dyn FeeRateProvider>,
        holdings: Arc<dyn Holdings>,
        ttl: Duration,
    ) -> Self {
        Self {
            network: rpc.network().into(),
            rpc,
            tokens,
            max_priority_fee,
            rates,
            holdings,
            signature: Cache::new(ttl),
            rent: Cache::new(Duration::MAX),
        }
    }

    /// The price of one signature: kept, else asked on `blockhash` or,
    /// without one, a recent one asked first. One call, or two.
    ///
    /// # Errors
    ///
    /// The node's failure.
    pub(crate) async fn per_signature(
        &self,
        blockhash: Option<Hash>,
    ) -> Result<u64, TransactionError> {
        self.signature
            .get(|| async {
                let blockhash = match blockhash {
                    Some(blockhash) => blockhash,
                    None => self.blockhash().await?.0,
                };
                let message = Message::new_with_blockhash(&[], Some(&PLACEHOLDER), &blockhash);
                self.rpc
                    .fee(&message)
                    .await
                    .map_err(TransactionError::unavailable)
            })
            .await
    }

    /// A recent confirmed blockhash, and the last block height a
    /// transaction on it can land at. One call.
    ///
    /// # Errors
    ///
    /// The node's failure.
    pub(crate) async fn blockhash(&self) -> Result<(Hash, u64), TransactionError> {
        self.rpc
            .blockhash()
            .await
            .map_err(TransactionError::unavailable)
    }

    /// The priority of a transaction of `limit` compute units at `rate`:
    /// its tip, never below [`FLOOR`], the price per unit.
    ///
    /// # Errors
    ///
    /// `FeeCap` above `max_priority_fee`.
    pub(crate) fn priority(
        &self,
        rate: base::FeeRate,
        limit: u32,
    ) -> Result<Priority, TransactionError> {
        let price = u64::try_from(rate.tip).unwrap_or(u64::MAX).max(FLOOR);
        if price > self.max_priority_fee {
            return Err(TransactionError::new(
                TransactionErrorKind::FeeCap,
                format!(
                    "Solana priority fee of {price} micro-lamports per compute unit exceeds the configured {}",
                    self.max_priority_fee
                ),
            ));
        }
        Ok(Priority { limit, price })
    }
}

impl base::Networked for Estimator {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl base::FeeEstimator for Estimator {
    /// Σ per spend one signature's fee and its priority fee at the rate
    /// now, plus, for a token, the rent of the recipient's token account
    /// (kept for good) unless it holds the token (it has its account then);
    /// no recipient: a new address, which has none. Each spend checked to
    /// be of this network: a canonical sender, SOL or an allowlisted token.
    fn estimate<'a>(&'a self, spends: &'a Spends) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let held = self.holdings.holds(spends).await?;
            let per_signature = self.per_signature(None).await?;
            let rate = self.rates.rate().await?;
            let mut total: u64 = 0;
            for (spend, held) in spends.iter().zip(held) {
                let parsed = Address::parse(&spend.from.value, self.rpc.network())?;
                ensure!(
                    *parsed.address() == spend.from,
                    "not a canonical address of this Solana network"
                );
                let token = self.tokens.contract(&spend.asset)?.is_some();
                let priority = self.priority(rate, limit(token))?;
                let mut fee = fee(per_signature, 1, priority)?;
                if token && !held {
                    let rent = self.rent.get(|| self.rpc.rent(TOKEN_ACCOUNT_BYTES)).await?;
                    fee = fee
                        .checked_add(rent)
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

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use base::FeeEstimator as _;
    use futures_executor::block_on;
    use rpc::testing::{Reply, Script};
    use serde_json::json;
    use solana_keypair::{Keypair, Signer as _};

    use super::Estimator;
    use crate::{Client, Config, Network, Tokens, fee_rate::RateProvider};

    const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    /// Holdings in which every recipient holds its asset while set.
    struct Holds(AtomicBool);

    impl base::Holdings for Holds {
        fn holds<'a>(
            &'a self,
            spends: &'a base::Spends,
        ) -> base::Future<'a, anyhow::Result<Vec<bool>>> {
            let held = self.0.load(Ordering::Relaxed);
            Box::pin(async move { Ok(vec![held; spends.len()]) })
        }
    }

    fn base58(byte: u8) -> String {
        bs58::encode([byte; 32]).into_string()
    }

    fn address(seed: u8) -> base::Address {
        let native = Network::new(base58(9).parse().unwrap(), "localnet");
        let key = Keypair::new_from_array([seed; 32]);
        base::Address::new((&native).into(), key.pubkey().to_string())
    }

    /// Recent priority fees of 1 000 micro-lamports per compute unit: 100
    /// lamports at a token transfer's 100 000 units.
    fn priority() -> Reply {
        Reply::ok(
            "getRecentPrioritizationFees",
            json!([{ "slot": 1, "prioritizationFee": 1000 }]),
        )
    }

    #[test]
    fn a_token_is_priced_with_rent_only_when_its_recipient_has_no_account() {
        let config = Config::new(Network::new(base58(9).parse().unwrap(), "localnet"));
        let script = Script::new([]);
        let rpc = Client::new(Arc::new(script.clone()), config.native());
        let rates = RateProvider {
            network: rpc.network().into(),
            rpc: rpc.clone(),
        };
        let holds = Arc::new(Holds(AtomicBool::new(true)));
        let estimator = Estimator::new(
            rpc,
            Tokens::new(address(1).network, [(MINT.parse().unwrap(), 6)]),
            1_000_000,
            Arc::new(rates),
            Arc::clone(&holds) as Arc<dyn base::Holdings>,
            Duration::from_secs(60),
        );
        let token = base::Asset {
            network: address(1).network,
            id: MINT.to_owned(),
        };
        let spends: base::Spends = [(address(1), token, address(2))].into_iter().collect();

        // The recipient holds the token, so has its account: a blockhash, a
        // signature at 5 000 lamports, and the priority fee.
        script.push([
            Reply::ok(
                "getLatestBlockhash",
                json!({ "context": { "slot": 1 }, "value": { "blockhash": base58(3), "lastValidBlockHeight": 50 } }),
            ),
            Reply::ok(
                "getFeeForMessage",
                json!({ "context": { "slot": 1 }, "value": 5000 }),
            ),
            priority(),
        ]);
        assert_eq!(
            block_on(estimator.estimate(&spends)).unwrap(),
            base::Decimal::from(5_100_u64)
        );
        // It does not: the rent of creating it too; the signature price is
        // kept.
        holds.0.store(false, Ordering::Relaxed);
        script.push([
            priority(),
            Reply::ok("getMinimumBalanceForRentExemption", json!(2_039_280)),
        ]);
        assert_eq!(
            block_on(estimator.estimate(&spends)).unwrap(),
            base::Decimal::from(5_100_u64 + 2_039_280)
        );
        script.exhausted();
    }
}

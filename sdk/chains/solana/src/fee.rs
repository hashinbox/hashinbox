//! [`Estimator`]: Solana's `base::FeeEstimator`, and the network state its
//! builders sign with.

use std::time::Duration;

use anyhow::{Context as _, ensure};
use base::{Addressed as _, Asset, Cache, Decimal, TransactionError};
use solana_hash::Hash;
use solana_message::Message;

use crate::{Address, Client, Tokens};

/// Bytes of an SPL Token account: what a recipient's new account holds.
const TOKEN_ACCOUNT_BYTES: u64 = 165;

/// The fee payer of the message that prices one signature; never signs.
const PLACEHOLDER: solana_address::Address = solana_address::Address::new_from_array([1; 32]);

/// The network state a send signs on.
#[derive(Clone, Debug)]
pub(crate) struct Prices {
    /// A recent confirmed blockhash.
    pub(crate) blockhash: Hash,
    /// Lamports per signature.
    pub(crate) per_signature: u64,
}

impl Prices {
    /// The fee of `message`: its signatures at the cached price.
    pub(crate) fn fee(&self, message: &Message) -> Result<u64, TransactionError> {
        self.per_signature
            .checked_mul(u64::from(message.header.num_required_signatures))
            .ok_or_else(|| TransactionError::failed("Solana fee overflows"))
    }
}

/// Solana's `base::FeeEstimator`: a blockhash and the price of a signature
/// cached for their time to live, and the rent of a token account kept from
/// its first read; every builder of the factory signs with them.
pub(crate) struct Estimator {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) tokens: Tokens,
    pub(crate) prices: Cache<Prices>,
    pub(crate) rent: Cache<u64>,
}

impl Estimator {
    /// Over `rpc`, sending SOL and `tokens`, prices cached `ttl`.
    pub(crate) fn new(rpc: Client, tokens: Tokens, ttl: Duration) -> Self {
        Self {
            network: rpc.network().into(),
            rpc,
            tokens,
            prices: Cache::new(ttl),
            rent: Cache::new(Duration::MAX),
        }
    }

    /// The cached prices, refreshed when stale: a blockhash, then the fee
    /// of a one-signature message on it.
    pub(crate) async fn prices(&self) -> Result<Prices, TransactionError> {
        self.prices
            .get(|| async {
                let blockhash = self
                    .rpc
                    .blockhash()
                    .await
                    .map_err(TransactionError::unavailable)?;
                let message = Message::new_with_blockhash(&[], Some(&PLACEHOLDER), &blockhash);
                let per_signature = self
                    .rpc
                    .fee(&message)
                    .await
                    .map_err(TransactionError::unavailable)?;
                Ok(Prices {
                    blockhash,
                    per_signature,
                })
            })
            .await
    }

    /// The rent-exempt minimum of a token account, in lamports.
    async fn rent(&self) -> anyhow::Result<u64> {
        self.rent.get(|| self.rpc.rent(TOKEN_ACCOUNT_BYTES)).await
    }
}

impl base::Networked for Estimator {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl base::FeeEstimator for Estimator {
    /// Σ per source one signature's fee, plus, for a token, the rent of the
    /// recipient's new token account (worst case: it has none). The fee leg
    /// a builder signs is the signature part; the rent is paid beside it.
    fn estimate<'a>(
        &'a self,
        sources: &'a [(base::Address, Asset)],
    ) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let native = Asset::native(self.network.clone());
            let mut total: u64 = 0;
            for (address, asset) in sources {
                let parsed = Address::parse(&address.value, self.rpc.network())?;
                ensure!(
                    parsed.address() == address,
                    "not a canonical address of this Solana network"
                );
                let mut fee = self.prices().await?.per_signature;
                if *asset != native {
                    ensure!(
                        asset.network == self.network && self.tokens.get(asset).is_some(),
                        "not SOL or an allowlisted token of this Solana network"
                    );
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

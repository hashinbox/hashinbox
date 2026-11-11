//! [`Estimator`]: Solana's `base::FeeEstimator`, and the network state its
//! builders sign with.

use anyhow::{Context as _, ensure};
use base::{Addressed as _, Asset, Decimal, TransactionError};
use solana_hash::Hash;
use solana_message::Message;

use crate::{Address, Client, Tokens, spl};

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

/// Solana's precise `base::FeeEstimator`: asks the node every time,
/// exactly for the senders, assets and recipients given; keeps nothing.
/// [`crate::fee_cache::Cached`] decides when its answers can be reused.
pub(crate) struct Estimator {
    pub(crate) network: base::Network,
    pub(crate) rpc: Client,
    pub(crate) tokens: Tokens,
}

impl Estimator {
    /// Over `rpc`, sending SOL and `tokens`.
    pub(crate) fn new(rpc: Client, tokens: Tokens) -> Self {
        Self {
            network: rpc.network().into(),
            rpc,
            tokens,
        }
    }

    /// A recent blockhash, and the price of one signature on it. Two calls.
    ///
    /// # Errors
    ///
    /// The node's failure.
    pub(crate) async fn prices(&self) -> Result<Prices, TransactionError> {
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
    }

    /// The rent-exempt minimum of a token account, in lamports. One call.
    ///
    /// # Errors
    ///
    /// The node's failure.
    pub(crate) async fn rent(&self) -> anyhow::Result<u64> {
        self.rpc.rent(TOKEN_ACCOUNT_BYTES).await
    }

    /// Whether `spend` sends a token (`false`: SOL), checked to be of this
    /// network: a canonical sender, SOL or an allowlisted token.
    ///
    /// # Errors
    ///
    /// Another network's sender or asset, or an asset not sent here.
    pub(crate) fn token(&self, spend: &base::Spend) -> anyhow::Result<bool> {
        let parsed = Address::parse(&spend.from.value, self.rpc.network())?;
        ensure!(
            *parsed.address() == spend.from,
            "not a canonical address of this Solana network"
        );
        if spend.asset == Asset::native(self.network.clone()) {
            return Ok(false);
        }
        ensure!(
            spend.asset.network == self.network && self.tokens.get(&spend.asset).is_some(),
            "not SOL or an allowlisted token of this Solana network"
        );
        Ok(true)
    }

    /// Whether `spend`'s recipient has its token account of `spend`'s token
    /// (finalized): then sending creates none. No recipient: a new address,
    /// which has none. One call.
    ///
    /// # Errors
    ///
    /// The node's failure.
    async fn has_account(&self, spend: &base::Spend) -> anyhow::Result<bool> {
        let (Some(to), Some(token)) = (&spend.to, self.tokens.get(&spend.asset)) else {
            return Ok(false);
        };
        let owner = Address::parse(&to.value, self.rpc.network())?;
        let account = spl::account(owner.as_native(), &token.mint)?;
        self.rpc.exists(&account).await
    }
}

impl base::Networked for Estimator {
    fn network(&self) -> &base::Network {
        &self.network
    }
}

impl base::FeeEstimator for Estimator {
    /// Σ per spend one signature's fee, plus, for a token, the rent of the
    /// recipient's token account when it has none: all asked now. The fee
    /// leg a builder signs is the signature part; the rent is paid beside
    /// it.
    fn estimate<'a>(
        &'a self,
        spends: &'a base::Spends,
    ) -> base::Future<'a, anyhow::Result<Decimal>> {
        Box::pin(async move {
            let per_signature = self.prices().await?.per_signature;
            let mut total: u64 = 0;
            for spend in spends.iter() {
                let mut fee = per_signature;
                if self.token(spend)? && !self.has_account(spend).await? {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use base::FeeEstimator as _;
    use futures_executor::block_on;
    use rpc::testing::{Reply, Script};
    use serde_json::json;
    use solana_keypair::{Keypair, Signer as _};

    use super::Estimator;
    use crate::{Client, Config, Network, Token, Tokens};

    const MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn base58(byte: u8) -> String {
        bs58::encode([byte; 32]).into_string()
    }

    fn address(seed: u8) -> base::Address {
        let native = Network::new(base58(9).parse().unwrap(), "localnet");
        let key = Keypair::new_from_array([seed; 32]);
        base::Address::new((&native).into(), key.pubkey().to_string())
    }

    /// A blockhash, then a signature at 5 000 lamports.
    fn prices() -> [Reply; 2] {
        [
            Reply::ok(
                "getLatestBlockhash",
                json!({ "context": { "slot": 1 }, "value": { "blockhash": base58(3), "lastValidBlockHeight": 50 } }),
            ),
            Reply::ok(
                "getFeeForMessage",
                json!({ "context": { "slot": 1 }, "value": 5000 }),
            ),
        ]
    }

    #[test]
    fn a_token_is_priced_with_rent_only_when_its_recipient_has_no_account() {
        let config = Config::new(Network::new(base58(9).parse().unwrap(), "localnet"));
        let script = Script::new([]);
        let estimator = Estimator::new(
            Client::new(Arc::new(script.clone()), config.native()),
            Tokens::new([Token {
                mint: MINT.parse().unwrap(),
                decimals: 6,
                ticker: None,
                display_decimals: None,
            }]),
        );
        let token = base::Asset {
            network: address(1).network,
            id: MINT.to_owned(),
        };
        let spends: base::Spends = [(address(1), token, address(2))].into_iter().collect();

        // The recipient's token account exists: one signature.
        script.push(prices());
        script.push([Reply::ok(
            "getAccountInfo",
            json!({ "context": { "slot": 1 }, "value": { "owner": base58(4), "data": ["", "base64"] } }),
        )]);
        assert_eq!(
            block_on(estimator.estimate(&spends)).unwrap(),
            base::Decimal::from(5_000_u64)
        );
        // It does not: the rent of creating it too, asked every time.
        script.push(prices());
        script.push([
            Reply::ok(
                "getAccountInfo",
                json!({ "context": { "slot": 1 }, "value": null }),
            ),
            Reply::ok("getMinimumBalanceForRentExemption", json!(2_039_280)),
        ]);
        assert_eq!(
            block_on(estimator.estimate(&spends)).unwrap(),
            base::Decimal::from(5_000_u64 + 2_039_280)
        );
        script.exhausted();
    }
}

//! [`Builder`]: one SOL or SPL token transfer on Solana, or its next
//! version.

use std::sync::Arc;

use base::{
    Addressed as _, Asset, Decimal, Fee, FeeRule as _, Future, Output, Signer, TransactionError,
    Transfer,
};
use serde::{Deserialize, Serialize};

use crate::{
    Address, Client, KeyPair, Token,
    fee::{fee, limit},
    fee_estimator::Estimator,
    transaction::{Transaction, message},
};

/// Pays one transfer of one asset (SOL, or the `token`) in one transaction,
/// from its own source or the default one, on a blockhash fetched as it
/// signs, at the price of its [`Fee`] over the network's rate; or the next
/// version of a loaded state: its payment and compute-unit limit, on a new
/// blockhash, its price at least a tenth above the old one. A signed
/// transaction lands only until its blockhash expires (about a minute), at
/// its `expires_height`; then it fails to
/// broadcast as `Expired`, and only then is a next version safe: nothing
/// else keeps the two from both landing.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    /// The factory's estimator: its cached signature price and rate.
    pub(crate) fees: Arc<Estimator>,
    pub(crate) asset: Asset,
    pub(crate) token: Option<Token>,
    pub(crate) default: Option<Signer>,
    /// What it pays, and its own source (`None`: the default).
    pub(crate) payment: Option<(Output<Address>, Option<Signer>)>,
    /// What the last sign chose, or a loaded one: the next sign continues it.
    pub(crate) state: Option<State>,
    pub(crate) fee: Fee,
}

/// What a signed payment chose: enough to sign its next version. No
/// blockhash: each version fetches its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct State {
    /// The recipient.
    to: String,
    /// Atomic units paid.
    amount: u64,
    /// Who paid and signed it.
    source: String,
    /// Its compute-unit limit.
    limit: u32,
    /// The rate it paid.
    rate: base::FeeRate,
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    /// Refuses a recipient that is off-curve, and anything but a positive
    /// `u64` of atomic units.
    fn from_transfer(&mut self, transfer: Transfer) -> Result<(), TransactionError> {
        let Transfer { to, amount, from } = transfer;
        let address =
            Address::parse(&to.value, self.rpc.network()).map_err(TransactionError::failed)?;
        if *address.address() != to {
            return Err(TransactionError::failed(
                "not a canonical address of this Solana network",
            ));
        }
        if !address.as_native().is_on_curve() {
            return Err(TransactionError::failed(
                "Solana recipient must be an on-curve address other than the sender",
            ));
        }
        if amount.to_atomic_u64(0).ok().is_none_or(|units| units == 0) {
            return Err(TransactionError::failed(
                "Solana amount must be a positive 64-bit number of atomic units",
            ));
        }
        let output = Output {
            address,
            asset: self.asset.clone(),
            amount,
        };
        self.payment = Some((output, from));
        self.state = None;
        Ok(())
    }

    fn with_fee(&mut self, fee: Fee) {
        self.fee = fee;
    }

    /// Refuses the sender as its own recipient. Above `max_priority_fee`:
    /// `FeeCap`.
    fn sign(&mut self) -> Future<'_, Result<base::Transaction, TransactionError>> {
        Box::pin(async move {
            let Some((output, from)) = &self.payment else {
                return Err(TransactionError::failed("no transfer to sign"));
            };
            let Some(signer) = from.as_ref().or(self.default.as_ref()) else {
                return Err(TransactionError::failed("the transfer has no source"));
            };
            if signer.address == *output.address.address() {
                return Err(TransactionError::failed(
                    "Solana recipient must be an on-curve address other than the sender",
                ));
            }
            let network = self.rpc.network();
            let key = KeyPair::from_signer(signer, network)?;
            let sender = key.native_address();
            let old = self.state.as_ref();
            if old.is_some_and(|old| old.source != key.address().value) {
                return Err(TransactionError::failed(
                    "the payment continued is not the source's",
                ));
            }
            let units = old.map_or_else(|| limit(self.token.is_some()), |old| old.limit);
            let (blockhash, expires_height) = self.fees.blockhash().await?;
            let per_signature = self.fees.per_signature(Some(blockhash.clone())).await?;
            let now = self.fees.rates.rate().await?;
            let rate = self.fee.rate(now, old.map(|old| old.rate));
            let rate = old.map_or(rate, |old| rate.higher(old.rate.times(1.1)));
            let priority = self.fees.priority(rate, units)?;
            let token = self.token.as_ref();
            let message = message(sender, token, output, &blockhash, priority)
                .map_err(TransactionError::failed)?;
            let fee = fee(
                per_signature,
                message.header.num_required_signatures,
                priority,
            )?;
            let mut transaction = Transaction::pending(message, sender, output, fee);
            transaction.sign(&key).map_err(TransactionError::failed)?;
            let state = State {
                to: output.address.address().value.clone(),
                amount: output
                    .amount
                    .to_atomic_u64(0)
                    .map_err(|error| TransactionError::failed(error.message))?,
                source: key.address().value.clone(),
                limit: units,
                rate,
            };
            self.state = Some(state);
            let mut signed = base::Transaction::from(transaction);
            signed.expires_height = Some(expires_height);
            Ok(signed)
        })
    }

    fn save(&self) -> Result<serde_json::Value, TransactionError> {
        let Some(state) = &self.state else {
            return Err(TransactionError::failed("nothing signed to save"));
        };
        serde_json::to_value(state).map_err(TransactionError::failed)
    }

    /// Its recipient and amount checked as [`Self::from_transfer`] does.
    fn load(&mut self, state: &serde_json::Value) -> Result<(), TransactionError> {
        let state: State =
            serde_json::from_value(state.clone()).map_err(TransactionError::failed)?;
        let to = base::Address::new(self.rpc.network().into(), state.to.clone());
        self.from_transfer(Transfer::new(to, Decimal::from(state.amount)))?;
        self.state = Some(state);
        Ok(())
    }
}

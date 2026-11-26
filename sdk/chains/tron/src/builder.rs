//! [`Builder`]: one TRX or TRC-20 transfer on Tron, or its next version.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use base::{
    Addressed as _, Asset, Decimal, Fee, FeeRule as _, Future, Output, Signer, Spends,
    TransactionError, Transfer,
};
use serde::{Deserialize, Serialize};

use crate::{
    Account, Address, Client, KeyPair,
    fee::Estimator,
    transaction::{Call, Transaction, raw},
    trc20,
};

/// Pays one transfer of one asset (TRX, or the `contract` token) in one
/// transaction, from its own source or the default one, referencing the
/// solidified block fetched as it signs; a token transfer burns at most its
/// `fee_limit`, the energy of [`crate::FeeLimits`] at the rate of its
/// [`Fee`]. Tron has no nonce: a transaction lands only until it expires,
/// about two minutes after signing, at its `expires_height`; then it fails
/// to broadcast as `Expired`, and only then is a next version (a loaded
/// state: its payment, on a new reference, its rate at least a tenth above
/// the old one) safe: nothing else keeps the two from both landing.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    /// The factory's estimator: its prices and limits.
    pub(crate) fees: Arc<Estimator>,
    pub(crate) asset: Asset,
    pub(crate) contract: Option<Account>,
    pub(crate) default: Option<Signer>,
    /// What it pays, and its own source (`None`: the default).
    pub(crate) payment: Option<(Output<Address>, Option<Signer>)>,
    /// What the last sign chose, or a loaded one: the next sign continues it.
    pub(crate) state: Option<State>,
    pub(crate) fee: Fee,
}

/// What a signed payment chose: enough to sign its next version. No
/// reference: each version fetches its own.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct State {
    /// The recipient.
    to: String,
    /// Atomic units paid, decimal text.
    amount: String,
    /// Who paid and signed it.
    source: String,
    /// The rate it paid.
    rate: base::FeeRate,
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    /// Refuses anything but a positive amount: of TRX a signed 64-bit
    /// number of sun, of a token a 256-bit one.
    fn from_transfer(&mut self, transfer: Transfer) -> Result<(), TransactionError> {
        let Transfer { to, amount, from } = transfer;
        let address =
            Address::parse(&to.value, self.rpc.network()).map_err(TransactionError::failed)?;
        if *address.address() != to {
            return Err(TransactionError::failed(
                "not a canonical address of this Tron network",
            ));
        }
        let valid = match self.contract {
            None => amount
                .to_atomic_u64(0)
                .is_ok_and(|sun| sun != 0 && i64::try_from(sun).is_ok()),
            Some(_) => amount
                .to_atomic_be_bytes::<32>(0)
                .is_ok_and(|word| word != [0; 32]),
        };
        if !valid {
            return Err(TransactionError::failed(
                "Tron amount must be a positive number of atomic units within range",
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

    /// Refuses TRX to the sender itself (Tron does). A `fee_limit` above
    /// `max_fee_limit`: `FeeCap`.
    fn sign(&mut self) -> Future<'_, Result<base::Transaction, TransactionError>> {
        Box::pin(async move {
            let Some((output, from)) = &self.payment else {
                return Err(TransactionError::failed("no transfer to sign"));
            };
            let Some(signer) = from.as_ref().or(self.default.as_ref()) else {
                return Err(TransactionError::failed("the transfer has no source"));
            };
            if self.contract.is_none() && signer.address == *output.address.address() {
                return Err(TransactionError::failed(
                    "Tron cannot send TRX to its own sender",
                ));
            }
            let key = KeyPair::from_signer(signer, self.rpc.network())?;
            let sender = key.native_address();
            let old = self.state.as_ref();
            if old.is_some_and(|old| old.source != key.address().value) {
                return Err(TransactionError::failed(
                    "the payment continued is not the source's",
                ));
            }
            let prices = self.fees.prices().await?;
            let now = base::FeeRate {
                base: prices.energy.into(),
                tip: 0,
            };
            let rate = self.fee.rate(now, old.map(|old| old.rate));
            let rate = old.map_or(rate, |old| rate.higher(old.rate.times(1.1)));
            let spend = (
                key.address().clone(),
                self.asset.clone(),
                output.address.address().clone(),
            );
            let held = self.fees.holds(&Spends::from_iter([spend])).await?;
            let to = *output.address.as_native();
            let (call, fee_limit) = match self.contract {
                None => {
                    let amount = output.amount.to_atomic_u64(0);
                    let amount = amount.map_err(|error| TransactionError::failed(error.message))?;
                    (Call::Transfer { to, amount }, None)
                }
                Some(contract) => {
                    let amount = output.amount.to_atomic_be_bytes::<32>(0);
                    let amount = amount.map_err(|error| TransactionError::failed(error.message))?;
                    let data = trc20::transfer(&to, amount);
                    let limit = self.fees.fee_limit(rate)?;
                    (Call::Trigger { contract, data }, Some(limit))
                }
            };
            let fee = Estimator::fee(prices, fee_limit, held.first() == Some(&true));
            let reference = self.rpc.reference().await?;
            let timestamp = now_ms().max(reference.timestamp);
            let raw = raw(sender.as_native(), &call, &reference, timestamp, fee_limit);
            let mut transaction = Transaction::pending(raw, sender, output, fee);
            transaction.sign(&key).map_err(TransactionError::failed)?;
            self.state = Some(State {
                to: output.address.address().value.clone(),
                amount: output.amount.to_string(),
                source: key.address().value.clone(),
                rate,
            });
            let mut signed = base::Transaction::from(transaction);
            signed.expires_height = Some(reference.expires_height());
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
        let amount: Decimal = state.amount.parse().map_err(TransactionError::failed)?;
        self.from_transfer(Transfer::new(to, amount))?;
        self.state = Some(state);
        Ok(())
    }
}

/// Unix milliseconds now, and above every earlier answer in this process:
/// two transactions signed in one millisecond on one reference still
/// differ.
fn now_ms() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    let last = LAST.fetch_max(now, Ordering::Relaxed);
    if last < now {
        return now;
    }
    LAST.fetch_add(1, Ordering::Relaxed) + 1
}

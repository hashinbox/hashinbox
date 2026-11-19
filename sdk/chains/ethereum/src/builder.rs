//! [`Builder`]: one EIP-1559 transaction for one Ethereum transfer, or its
//! replacement.

use std::sync::Arc;

use alloy_consensus::TxEip1559;
use base::{
    Addressed as _, Asset, Fee, FeeRule as _, Future, Output, Signer, TransactionError, Transfer,
};
use serde::{Deserialize, Serialize};

use crate::{
    Address, Client, KeyPair,
    fee::{fee_cap, max_fee},
    fee_estimator::Estimator,
    transaction::{Transaction, request, signed},
};

/// Pays one transfer of one asset (ether, or the `contract` token) in one
/// transaction, from its own source or the default one, at the sender's
/// next nonce ([`Builder::nonce`]) and the gas the node estimates for it;
/// or replaces a loaded state: its nonce,
/// payment and gas limit, both fees at least a tenth above its own (what a
/// node takes as a replacement).
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    /// The sends in flight: the nonces they hold.
    pub(crate) transactions: Arc<dyn base::TransactionIndexStore>,
    /// The factory's estimator: its cached rate and its limits price the
    /// transfer; the gas is the node's estimate of this very payment.
    pub(crate) fees: Arc<Estimator>,
    pub(crate) asset: Asset,
    pub(crate) contract: Option<alloy_primitives::Address>,
    pub(crate) default: Option<Signer>,
    /// What it pays, and its own source (`None`: the default).
    pub(crate) payment: Option<(Output<Address>, Option<Signer>)>,
    /// What the last sign chose, or a loaded one: the next sign replaces it.
    pub(crate) state: Option<State>,
    pub(crate) fee: Fee,
}

/// What a signed payment chose: enough to replace it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct State {
    /// The recipient.
    to: String,
    /// Atomic units paid, decimal text.
    amount: String,
    /// Who signed it.
    source: String,
    nonce: u64,
    gas_limit: u64,
    /// The rate it paid.
    rate: base::FeeRate,
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    fn from_transfer(&mut self, transfer: Transfer) -> Result<(), TransactionError> {
        let Transfer { to, amount, from } = transfer;
        let address =
            Address::parse(&to.value, self.rpc.network()).map_err(TransactionError::failed)?;
        if *address.address() != to {
            return Err(TransactionError::failed(
                "not a canonical address of this Ethereum network",
            ));
        }
        if amount
            .to_atomic_be_bytes::<32>(0)
            .ok()
            .is_none_or(|word| word == [0; 32])
        {
            return Err(TransactionError::failed(
                "Ethereum amount must be a positive 256-bit number of atomic units",
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

    /// Above the fee limits (each fee per gas, the worst-case total):
    /// `FeeCap`.
    fn sign(&mut self) -> Future<'_, Result<base::Transaction, TransactionError>> {
        Box::pin(async move {
            let Some((output, from)) = &self.payment else {
                return Err(TransactionError::failed("no transfer to sign"));
            };
            let Some(signer) = from.as_ref().or(self.default.as_ref()) else {
                return Err(TransactionError::failed("the transfer has no source"));
            };
            let network = self.rpc.network();
            let key = KeyPair::from_signer(signer, network)?;
            let sender = key.native_address();
            let old = self.state.as_ref();
            if old.is_some_and(|old| old.source != key.address().value) {
                return Err(TransactionError::failed(
                    "the payment continued is not the source's",
                ));
            }
            let mut native = request(output, self.contract).map_err(TransactionError::failed)?;
            let rates = &self.fees.rates;
            let (nonce, gas_limit, now) = match old {
                Some(old) => (old.nonce, old.gas_limit, rates.rate().await?),
                None => {
                    let nonce = self.nonce(&key).await?;
                    let now = rates.rate().await?;
                    (nonce, self.gas(sender, &native).await?, now)
                }
            };
            let rate = self.fee.rate(now, old.map(|old| old.rate));
            let rate = old.map_or(rate, |old| rate.higher(old.rate.times(1.1)));
            self.fees.limits.total(gas_limit, rate).map_err(fee_cap)?;
            native.chain_id = network.chain_id;
            native.nonce = nonce;
            native.gas_limit = gas_limit;
            native.max_fee_per_gas = max_fee(rate);
            native.max_priority_fee_per_gas = rate.tip;
            let mut transaction = Transaction::pending(native, sender, output);
            transaction.sign(&key).map_err(TransactionError::failed)?;
            let state = State {
                to: output.address.address().value.clone(),
                amount: output.amount.to_string(),
                source: key.address().value.clone(),
                nonce,
                gas_limit,
                rate,
            };
            self.state = Some(state);
            Ok(transaction.into())
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
        let amount = state.amount.parse().map_err(TransactionError::failed)?;
        self.from_transfer(Transfer::new(to, amount))?;
        self.state = Some(state);
        Ok(())
    }
}

impl Builder {
    /// `key`'s next nonce: the node's pending count, or one above the
    /// highest nonce of its own sends in flight when that is higher,
    /// so a node whose mempool misses one never hands its nonce out again.
    async fn nonce(&self, key: &KeyPair) -> Result<u64, TransactionError> {
        let counted = self
            .rpc
            .nonce(key.native_address())
            .await
            .map_err(TransactionError::unavailable)?;
        let sent = self
            .transactions
            .find_sent(key.address())
            .await
            .map_err(TransactionError::unavailable)?;
        sent.iter().try_fold(counted, |next, transaction| {
            let (native, _) = signed(&transaction.bytes).map_err(TransactionError::failed)?;
            Ok(next.max(native.nonce.saturating_add(1)))
        })
    }

    /// The gas limit of `sender` sending `native`: the node's estimate of
    /// that very payment plus the margin, never a kept one. A payment that
    /// would revert fails `Reason::Reverted`, one the sender cannot fund
    /// `Reason::InsufficientFunds`, as the node says.
    async fn gas(&self, sender: &Address, native: &TxEip1559) -> Result<u64, TransactionError> {
        let estimated = self.rpc.estimate(sender, native).await?;
        self.fees
            .limits
            .gas_limit(estimated)
            .map_err(TransactionError::failed)
    }
}

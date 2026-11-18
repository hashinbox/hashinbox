//! [`Builder`]: one Bitcoin transfer in one transaction, or its replacement.

use std::sync::Arc;

use base::{
    Addressed as _, Asset, Coin, Decimal, Fee, FeeRateProvider, FeeRule as _, Future, Input,
    InputStore, Inputs, Output, Reason, Signer, TransactionError, TransactionId, Transfer,
};
use serde::{Deserialize, Serialize};

use crate::{Address, Network};
use crate::{
    FeeRate, KeyPair, fee::vsize, rpc::Client, transaction::Transaction as NativeTransaction,
};

/// Pays one transfer in one transaction: its fee's rate over the factory's
/// rate now, never below the floor, the sender's coins largest first, change back to the
/// default source, else the sender, only when it is worth its own output
/// (above dust after paying for itself); a smaller leftover is fee. A
/// loaded state is replaced: the same coins and outputs, its change paying
/// the higher fee.
pub(crate) struct Builder {
    pub(crate) rpc: Client,
    pub(crate) inputs: Arc<dyn InputStore>,
    /// The factory's cached rate, which prices the fee.
    pub(crate) rates: Arc<dyn FeeRateProvider>,
    pub(crate) floor: FeeRate,
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
    /// Satoshis paid to it.
    amount: u64,
    /// Who paid: the address of the key whose coins it spends.
    source: String,
    /// The coins spent, in order.
    inputs: Vec<Spent>,
    /// Where its change went; `None`: it had none (the leftover was fee).
    change: Option<String>,
    /// The rate it paid.
    rate: base::FeeRate,
}

/// One coin a payment spends.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Spent {
    txid: String,
    vout: u32,
    amount: u64,
}

impl Builder {
    /// Every unspent coin of `key`, owned by its native address.
    async fn coins(&self, key: &KeyPair) -> Result<Inputs<Address>, TransactionError> {
        let found = self
            .inputs
            .find_unspent(std::slice::from_ref(key.address()))
            .await
            .map_err(TransactionError::unavailable)?;
        let coins: Vec<_> = found
            .into_iter()
            .filter(|input| input.address == *key.address())
            .map(|input| Input {
                coin: input.coin,
                address: key.native_address().clone(),
                asset: input.asset,
                amount: input.amount,
                height: input.height,
            })
            .collect();
        Ok(coins.into())
    }

    /// `rate`, its tip raised so its total is at least the floor.
    fn floored(&self, rate: base::FeeRate) -> base::FeeRate {
        let floor = u128::from(self.floor.sat_per_kvb());
        base::FeeRate {
            tip: rate.tip.max(floor.saturating_sub(rate.base)),
            ..rate
        }
    }

    /// A new payment of `output` from `key`'s coins at `rate`.
    async fn first(
        &self,
        key: &KeyPair,
        output: &Output<Address>,
        rate: base::FeeRate,
    ) -> Result<(NativeTransaction, State), TransactionError> {
        let network = self.rpc.network();
        let owner = match &self.default {
            Some(signer) => KeyPair::from_signer(signer, network)?
                .native_address()
                .clone(),
            None => key.native_address().clone(),
        };
        let inputs = self.coins(key).await?;
        let (own, paid) = (owner.script_pubkey(), output.address.script_pubkey());
        let fee = |kept: &Inputs<Address>, change: bool| {
            let spent: Vec<_> = kept
                .iter()
                .map(|input| input.address.script_pubkey())
                .collect();
            let outputs = std::iter::once(&paid).chain(change.then_some(&own));
            Decimal::from(FeeRate::from(rate).fee(vsize(
                spent.iter().map(AsRef::as_ref),
                outputs.map(AsRef::as_ref),
            )))
        };
        let requested = output.amount.clone();
        let chosen = inputs
            .largest_first()
            .filter(|kept, _| kept.total() < &requested + &fee(kept, false));
        if chosen.total() < &requested + &fee(&chosen, false) {
            return Err(short("insufficient funds for the transfer and fee"));
        }
        let dust = Decimal::from(own.minimal_non_dust().to_sat());
        let with_change = &requested + &fee(&chosen, true);
        let change = (chosen.total() >= &with_change + &dust).then(|| Output {
            address: owner.clone(),
            asset: Asset::native(network.into()),
            amount: chosen.total() - &with_change,
        });
        let transaction = signed(key, &chosen, output, change.as_ref(), network)?;
        let state = State {
            to: output.address.address().value.clone(),
            amount: sats(&output.amount)?,
            source: key.address().value.clone(),
            inputs: chosen
                .iter()
                .map(|input| {
                    Ok(Spent {
                        txid: input.coin.transaction.id.clone(),
                        vout: input.coin.index,
                        amount: sats(&input.amount)?,
                    })
                })
                .collect::<Result<_, TransactionError>>()?,
            change: change.map(|change| change.address.address().value.clone()),
            rate,
        };
        Ok((transaction, state))
    }

    /// The replacement of `state` at `rate`: its coins and outputs, its
    /// change less the extra fee. A node takes it at a higher rate paying at
    /// least the old fee plus the rate's base over its size.
    fn bump(
        &self,
        key: &KeyPair,
        output: &Output<Address>,
        state: &State,
        rate: base::FeeRate,
    ) -> Result<(NativeTransaction, State), TransactionError> {
        let Some(change) = &state.change else {
            return Err(short("no change to pay a higher fee"));
        };
        let network = self.rpc.network();
        let owner = Address::parse(change, network).map_err(TransactionError::failed)?;
        let asset = Asset::native(network.into());
        let coins: Vec<_> = state
            .inputs
            .iter()
            .map(|spent| Input {
                coin: Coin {
                    transaction: TransactionId::new(network.into(), spent.txid.clone()),
                    index: spent.vout,
                },
                address: key.native_address().clone(),
                asset: asset.clone(),
                amount: spent.amount.into(),
                height: 0,
            })
            .collect();
        let chosen = Inputs::from(coins);
        let (own, paid) = (owner.script_pubkey(), output.address.script_pubkey());
        let spent: Vec<_> = chosen
            .iter()
            .map(|input| input.address.script_pubkey())
            .collect();
        let size = vsize(spent.iter().map(AsRef::as_ref), [&*paid, &*own]);
        let step = FeeRate::from_sat_per_kvb(u64::try_from(rate.base).unwrap_or(u64::MAX));
        let fee = FeeRate::from(rate).fee(size).max(
            FeeRate::from(state.rate)
                .fee(size)
                .saturating_add(step.fee(size)),
        );
        let dust = own.minimal_non_dust().to_sat();
        let total: u64 = state.inputs.iter().map(|spent| spent.amount).sum();
        let Some(left) = total
            .checked_sub(state.amount.saturating_add(fee))
            .filter(|left| *left >= dust)
        else {
            return Err(short("the change cannot pay a higher fee"));
        };
        let change = Output {
            address: owner,
            asset,
            amount: left.into(),
        };
        let transaction = signed(key, &chosen, output, Some(&change), network)?;
        let paid = u128::from(fee.saturating_mul(1_000).div_ceil(size));
        let state = State {
            rate: base::FeeRate {
                tip: paid.saturating_sub(rate.base),
                ..rate
            },
            ..state.clone()
        };
        Ok((transaction, state))
    }
}

/// `chosen` paying `output` and `change`, signed by `key`.
fn signed(
    key: &KeyPair,
    chosen: &Inputs<Address>,
    output: &Output<Address>,
    change: Option<&Output<Address>>,
    network: Network,
) -> Result<NativeTransaction, TransactionError> {
    let mut transaction =
        NativeTransaction::from_inputs(chosen, std::iter::once(output).chain(change), network)
            .map_err(TransactionError::failed)?;
    transaction
        .sign(std::slice::from_ref(key))
        .map_err(TransactionError::failed)?;
    Ok(transaction)
}

/// Short of funds: `message`.
fn short(message: &str) -> TransactionError {
    TransactionError::failed(message).with_reason(Reason::InsufficientFunds)
}

/// `amount` in satoshis.
fn sats(amount: &Decimal) -> Result<u64, TransactionError> {
    amount
        .to_atomic_u64(0)
        .map_err(|error| TransactionError::failed(error.message))
}

impl base::Builder for Builder {
    fn with_default_source(&mut self, source: Signer) {
        self.default = Some(source);
    }

    fn from_transfer(&mut self, transfer: Transfer) -> Result<(), TransactionError> {
        let Transfer { to, amount, from } = transfer;
        let network = self.rpc.network();
        let address = Address::parse(&to.value, network).map_err(TransactionError::failed)?;
        if *address.address() != to {
            return Err(TransactionError::failed(
                "not a canonical address of this Bitcoin network",
            ));
        }
        if amount.to_atomic_u64(0).ok().is_none_or(|value| value == 0) {
            return Err(TransactionError::failed(
                "Bitcoin amount must be a positive number of satoshis",
            ));
        }
        let output = Output {
            address,
            asset: Asset::native(network.into()),
            amount,
        };
        self.payment = Some((output, from));
        self.state = None;
        Ok(())
    }

    fn with_fee(&mut self, fee: Fee) {
        self.fee = fee;
    }

    fn sign(&mut self) -> Future<'_, Result<base::Transaction, TransactionError>> {
        Box::pin(async move {
            let Some((output, from)) = &self.payment else {
                return Err(TransactionError::failed("no transfer to sign"));
            };
            let Some(signer) = from.as_ref().or(self.default.as_ref()) else {
                return Err(TransactionError::failed("the transfer has no source"));
            };
            let key = KeyPair::from_signer(signer, self.rpc.network())?;
            let old = self.state.as_ref();
            if old.is_some_and(|old| old.source != key.address().value) {
                return Err(TransactionError::failed(
                    "the payment continued is not the source's",
                ));
            }
            let now = self.rates.rate().await?;
            let rate = self.floored(self.fee.rate(now, old.map(|old| old.rate)));
            let (transaction, state) = match old {
                Some(old) => self.bump(&key, output, old, rate)?,
                None => self.first(&key, output, rate).await?,
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
        self.from_transfer(Transfer::new(to, state.amount.into()))?;
        self.state = Some(state);
        Ok(())
    }
}

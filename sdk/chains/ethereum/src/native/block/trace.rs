//! Per-transaction balance changes, as the node's own execution reports them.
//!
//! The evidence is one `debug_traceBlockByHash` result taken with the
//! `prestateTracer` in `diffMode`: for each transaction, the accounts it
//! modified with their balance before and after. That is the shape Solana's
//! `preBalances`/`postBalances` already have, so both chains read the
//! runtime's own accounting as the truth and treat the decoded instructions —
//! here the top-level `value` and the ERC-20 `Transfer` logs — as attribution
//! over it.

use std::collections::BTreeMap;

use serde_json::Value;

use super::field::{Object, required_hash, required_quantity};
use super::head::Head;
use crate::native::{Address, Error, TransactionId, Wei};

/// The ether balance of one account before and after a transaction executed.
///
/// `after` is total: it already carries the gas the sender paid and the
/// priority fee the block's beneficiary received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Balance {
    /// Before execution.
    pub before: Wei,
    /// After execution, fee included.
    pub after: Wei,
}

/// Every account whose ether balance one transaction changed, in address
/// order.
///
/// Accounts the transaction only read, and accounts whose balance ends where
/// it started, are absent: neither can move a balance.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trace {
    balances: BTreeMap<Address, Balance>,
}

impl Trace {
    /// How `address` changed, or `None` when its balance did not change.
    #[must_use]
    pub fn balance(&self, address: &Address) -> Option<&Balance> {
        self.balances.get(address)
    }

    /// Every account whose balance changed, with its change, in address order.
    pub fn changes(&self) -> impl Iterator<Item = (&Address, &Balance)> {
        self.balances.iter()
    }

    /// Splits one `debug_traceBlockByHash` result into one [`Trace`] per
    /// transaction of `head`, in block order.
    ///
    /// The result must carry exactly one entry per transaction: that, with the
    /// `txHash` check below, is what reconciles a trace against the block it
    /// claims to describe.
    ///
    /// # Errors
    ///
    /// [`crate::native::ErrorKind::InvalidBlock`] naming the first violation.
    pub(crate) fn parse_all(raw: &[u8], head: &Head) -> Result<Vec<Self>, Error> {
        let entries: Vec<Value> = serde_json::from_slice(raw).map_err(|_| {
            Error::invalid_block("Ethereum block trace must be an array of transaction traces")
        })?;
        if entries.len() != head.transactions.len() {
            return Err(Error::invalid_block(
                "Ethereum block trace count does not match transaction count",
            ));
        }
        head.ids()
            .zip(&entries)
            .map(|(id, entry)| Self::parse(entry, id))
            .collect()
    }

    /// One `{ txHash, result: { pre, post } }` entry.
    ///
    /// `txHash` is checked when the client sends it; the `trace_`-lineage
    /// clients that omit it are still pinned by position and count.
    fn parse(entry: &Value, id: &TransactionId) -> Result<Self, Error> {
        let entry: &Object = entry
            .as_object()
            .ok_or_else(|| Error::invalid_block("Ethereum transaction trace must be an object"))?;
        if entry.get("error").is_some_and(|error| !error.is_null()) {
            return Err(Error::invalid_block(
                "Ethereum node could not trace one transaction of the block",
            ));
        }
        if entry.contains_key("txHash")
            && &required_hash(entry, "txHash", "transaction trace hash")? != id.as_bytes()
        {
            return Err(Error::invalid_block(
                "Ethereum transaction trace hash does not match block order",
            ));
        }
        let result = entry
            .get("result")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                Error::invalid_block("Ethereum transaction trace carries no prestate result")
            })?;
        Self::diff(&Self::state(result, "pre")?, &Self::state(result, "post")?)
    }

    /// One side of a `diffMode` prestate: each named account with the balance
    /// that side reports for it, if it reports one.
    ///
    /// Both sides are required. A node that ignores `diffMode` answers with a
    /// bare prestate and no `post` key, which would otherwise read as every
    /// touched account being emptied.
    fn state(result: &Object, side: &str) -> Result<BTreeMap<Address, Option<Wei>>, Error> {
        let accounts = result
            .get(side)
            .and_then(Value::as_object)
            .ok_or_else(|| Error::invalid_block(Self::ignored(side)))?;
        accounts
            .iter()
            .map(|(address, account)| Self::account(address, account))
            .collect()
    }

    fn account(address: &str, account: &Value) -> Result<(Address, Option<Wei>), Error> {
        let address = address.parse::<Address>().map_err(|_| {
            Error::invalid_block("Ethereum trace account is not a canonical 20-byte address")
        })?;
        let account: &Object = account
            .as_object()
            .ok_or_else(|| Error::invalid_block("Ethereum trace account must be an object"))?;
        // Only a hex QUANTITY is accepted. Clients old enough to encode the
        // balance as a JSON number are rejected here and by the startup probe,
        // rather than read with a second grammar.
        let balance = match account.get("balance") {
            None | Some(Value::Null) => None,
            Some(Value::String(_)) => Some(required_quantity(
                account,
                "balance",
                "trace account balance",
            )?),
            Some(_) => {
                return Err(Error::invalid_block(
                    "Ethereum trace account balance must be a hex quantity string",
                ));
            }
        };
        Ok((address, balance.map(Wei::from_u256)))
    }

    fn ignored(side: &str) -> String {
        format!("Ethereum transaction trace has no `{side}` state, so it is not a diff")
    }

    /// Pairs the two sides into one balance change per account.
    ///
    /// `pre` names every account the transaction modified with the state it
    /// had, so an account absent from `pre` was created and started at zero.
    /// `post` names only the fields that changed, so an account there without
    /// a `balance` kept the one it had, and an account missing from `post`
    /// altogether was destroyed and holds nothing.
    fn diff(
        pre: &BTreeMap<Address, Option<Wei>>,
        post: &BTreeMap<Address, Option<Wei>>,
    ) -> Result<Self, Error> {
        let mut balances = BTreeMap::new();
        for address in pre.keys().chain(post.keys()) {
            let destroyed = pre.contains_key(address) && !post.contains_key(address);
            let after = if destroyed {
                Some(Wei::ZERO)
            } else {
                post.get(address).cloned().flatten()
            };
            let Some(after) = after else {
                continue;
            };
            let before = Self::before(pre, address)?;
            if before != after {
                balances.insert(address.clone(), Balance { before, after });
            }
        }
        Ok(Self { balances })
    }

    /// The balance an account held before the transaction: zero when the
    /// transaction created it, and an error when the node reports a new
    /// balance for an account whose prior balance it withheld.
    fn before(pre: &BTreeMap<Address, Option<Wei>>, address: &Address) -> Result<Wei, Error> {
        match pre.get(address) {
            None => Ok(Wei::ZERO),
            Some(Some(before)) => Ok(before.clone()),
            Some(None) => Err(Error::invalid_block(
                "Ethereum trace reports a new account balance without its prior balance",
            )),
        }
    }
}

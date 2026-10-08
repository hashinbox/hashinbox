use base::BoxFuture;
use serde_json::{Value, json};

use super::{FeeRate, SignedTransaction, TransactionId};
use crate::native::{Error, ErrorKind, Satoshi, json::Object, rpc::Client};

/// Bitcoin Core's `testmempoolaccept` verdict on one transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acceptance {
    /// Whether the mempool would accept it now.
    pub allowed: bool,
    /// Core's reason when it would not.
    pub reject_reason: Option<String>,
    /// Its virtual size, when reported.
    pub virtual_size: Option<u64>,
    /// Its fee, when reported.
    pub base_fee: Option<Satoshi>,
}

/// The node checks a send needs before anything is submitted: the fee rate
/// to build with, and whether the mempool would take the signed result.
pub trait Preflight: Send + Sync {
    /// Bitcoin Core's conservative fee rate for confirmation within
    /// `target_blocks` blocks, refused above `maximum`.
    fn fee_rate(
        &self,
        target_blocks: u16,
        maximum: FeeRate,
    ) -> BoxFuture<'_, Result<FeeRate, Error>>;

    /// Asks the node whether it would accept `transaction` at or below
    /// `maximum`, without submitting it.
    fn accept<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        maximum: FeeRate,
    ) -> BoxFuture<'a, Result<Acceptance, Error>>;
}

impl<C: json_rpc::Client> Preflight for Client<C> {
    fn fee_rate(
        &self,
        target_blocks: u16,
        maximum: FeeRate,
    ) -> BoxFuture<'_, Result<FeeRate, Error>> {
        Box::pin(async move { self.estimate(target_blocks).await?.within(maximum) })
    }

    fn accept<'a>(
        &'a self,
        transaction: &'a SignedTransaction,
        maximum: FeeRate,
    ) -> BoxFuture<'a, Result<Acceptance, Error>> {
        Box::pin(async move {
            let params = json!([[transaction.hex()], maximum.core_maximum()?]);
            let reply = self.request("testmempoolaccept", params).await?;
            Acceptance::parse(&reply, transaction.id)
        })
    }
}

impl FeeRate {
    /// This rate, unless it exceeds `maximum`.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::FeeUnavailable`] above `maximum`.
    pub fn within(self, maximum: Self) -> Result<Self, Error> {
        if self > maximum {
            return Err(Error::new(
                ErrorKind::FeeUnavailable,
                "estimated Bitcoin fee rate exceeds the configured maximum",
            ));
        }
        Ok(self)
    }
}

impl Acceptance {
    /// Nothing when the mempool would accept the transaction.
    ///
    /// # Errors
    ///
    /// [`ErrorKind::Rejected`] otherwise: submitting it would fail.
    pub fn require(&self) -> Result<(), Error> {
        if !self.allowed {
            return Err(Error::new(
                ErrorKind::Rejected,
                "Bitcoin node rejected transaction preflight",
            ));
        }
        Ok(())
    }

    fn parse(reply: &Value, expected: TransactionId) -> Result<Self, Error> {
        let [result] = reply.as_array().map(Vec::as_slice).unwrap_or_default() else {
            return Err(Error::invalid_reply(
                "Bitcoin testmempoolaccept returned an unexpected result count",
            ));
        };
        let result = Object::new(
            result,
            "Bitcoin testmempoolaccept result",
            Error::invalid_reply,
        )?;
        let returned = result
            .string("txid", "Bitcoin preflight transaction ID")?
            .parse::<TransactionId>()
            .map_err(|_| result.fail("Bitcoin preflight returned an invalid txid"))?;
        if returned != expected {
            return Err(result.fail("Bitcoin preflight returned a different transaction ID"));
        }
        let allowed = result.bool("allowed", "Bitcoin preflight allowance")?;
        let base_fee = result
            .get("fees")
            .and_then(|fees| fees.get("base"))
            .map(|fee| Satoshi::from_btc(fee, "Bitcoin preflight base fee"))
            .transpose()
            .map_err(Error::invalid_reply)?;
        Ok(Self {
            allowed,
            reject_reason: result
                .get("reject-reason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            virtual_size: result.get("vsize").and_then(Value::as_u64),
            base_fee,
        })
    }
}

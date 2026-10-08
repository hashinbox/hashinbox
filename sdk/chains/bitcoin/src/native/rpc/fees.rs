use base::BoxFuture;
use serde_json::json;

use super::Client;
use crate::native::{Error, ErrorKind, Satoshi, json::Object, transaction::FeeRate};

/// Fee estimation: what a wallet pays per virtual byte.
impl<C: json_rpc::Client> Client<C> {
    /// Bitcoin Core's conservative estimate for confirmation within
    /// `target_blocks` blocks.
    ///
    /// # Errors
    ///
    /// `InvalidConfiguration` for a zero target, `FeeUnavailable` when Core
    /// has no estimate, and an RPC error for a zero or malformed rate.
    pub fn estimate<'a>(&'a self, target_blocks: u16) -> BoxFuture<'a, Result<FeeRate, Error>> {
        Box::pin(async move {
            if target_blocks == 0 {
                return Err(Error::new(
                    ErrorKind::InvalidConfiguration,
                    "Bitcoin fee-estimation target must be greater than zero",
                ));
            }
            let reply = self
                .request("estimatesmartfee", json!([target_blocks, "conservative"]))
                .await?;
            let reply = Object::new(
                &reply,
                "Bitcoin estimatesmartfee result",
                Error::invalid_reply,
            )?;
            let rate = reply.get("feerate").ok_or_else(|| {
                Error::new(
                    ErrorKind::FeeUnavailable,
                    "Bitcoin Core cannot currently estimate a fee rate",
                )
            })?;
            let rate = Satoshi::from_btc(rate, "Bitcoin estimated BTC/kvB fee rate")
                .map_err(Error::invalid_reply)?;
            if rate.units() == 0 {
                return Err(Error::rpc("Bitcoin Core estimated a zero fee rate", true));
            }
            Ok(FeeRate::from_sat_per_kvb(rate.units()))
        })
    }
}

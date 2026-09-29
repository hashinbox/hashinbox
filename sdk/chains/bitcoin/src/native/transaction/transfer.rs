use bitcoin::ScriptBuf;

use crate::native::{Address, Error, Network, Satoshi};

/// One requested payment, in native types. Never an observed movement.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Transfer {
    /// The recipient.
    pub to: Address,
    /// Satoshis to pay; a drain transfer's amount is computed by the builder.
    pub amount: Satoshi,
}

impl Transfer {
    /// `amount` to `to`.
    #[must_use]
    pub const fn new(to: Address, amount: Satoshi) -> Self {
        Self { to, amount }
    }

    /// The recipient's script on `network`, rejecting dust unless `allow_zero`
    /// (a drain output whose amount is not known yet).
    pub(super) fn script(&self, network: Network, allow_zero: bool) -> Result<ScriptBuf, Error> {
        let script = self.to.script_pubkey(network)?;
        let minimum = script.minimal_non_dust().to_sat();
        if !allow_zero && self.amount.units() < minimum {
            return Err(Error::invalid_transaction(format!(
                "Bitcoin recipient output is dust: minimum is {minimum} satoshis"
            )));
        }
        Ok(script)
    }
}

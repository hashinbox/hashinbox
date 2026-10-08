use super::{BuildContext, Transfer};
use crate::native::{Address, Error, ErrorKind, Wei};

/// A built, unsigned EIP-1559 transaction: every field that will be signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction {
    /// The chain, sender, nonce, gas limit and fees it was built under.
    pub context: BuildContext,
    /// Recipient or contract; `None` creates a contract.
    pub to: Option<Address>,
    /// Ether attached.
    pub value: Wei,
    /// Calldata.
    pub input: Vec<u8>,
}

impl UnsignedTransaction {
    pub(super) fn new(transfer: &Transfer, context: &BuildContext) -> Result<Self, Error> {
        if context.chain_id == 0 {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum chain ID must be non-zero",
            ));
        }
        if context.gas_limit == 0 {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum gas limit must be non-zero",
            ));
        }
        if context.max_fee_per_gas < context.max_priority_fee_per_gas {
            return Err(Error::new(
                ErrorKind::InvalidTransaction,
                "Ethereum max fee per gas is below the priority fee",
            ));
        }
        Ok(Self {
            context: context.clone(),
            to: Some(transfer.target().clone()),
            value: transfer.value(),
            input: transfer.input(),
        })
    }
}

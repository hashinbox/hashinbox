use super::{BuildContext, Transfer};
use crate::native::{Address, Error, ErrorKind, Wei};

/// A built, unsigned EIP-1559 transaction: every field that will be signed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsignedTransaction {
    /// The chain the transaction signs for.
    pub chain_id: u64,
    /// The sender's nonce.
    pub nonce: u64,
    /// The sender; the signature must recover to it.
    pub from: Address,
    /// Recipient or contract; `None` creates a contract.
    pub to: Option<Address>,
    /// Ether attached.
    pub value: Wei,
    /// Calldata.
    pub input: Vec<u8>,
    /// Gas limit.
    pub gas_limit: u64,
    /// Max fee per gas.
    pub max_fee_per_gas: Wei,
    /// Priority fee per gas.
    pub max_priority_fee_per_gas: Wei,
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
            chain_id: context.chain_id,
            nonce: context.nonce,
            from: context.from.clone(),
            to: Some(transfer.target().clone()),
            value: transfer.value(),
            input: transfer.input(),
            gas_limit: context.gas_limit,
            max_fee_per_gas: context.max_fee_per_gas.clone(),
            max_priority_fee_per_gas: context.max_priority_fee_per_gas.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Transfer {
        Transfer {
            asset: crate::native::AssetKind::Native,
            to: Address::from_bytes([0x22; 20]),
            amount: Wei::from_u128(7),
        }
    }

    fn context() -> BuildContext {
        BuildContext {
            chain_id: 1,
            from: Address::from_bytes([0x11; 20]),
            nonce: 2,
            gas_limit: 21_000,
            max_fee_per_gas: Wei::from_u128(10),
            max_priority_fee_per_gas: Wei::from_u128(3),
        }
    }

    #[test]
    fn build_preserves_chain_native_fields() {
        let request = request();
        let context = context();
        let transaction =
            UnsignedTransaction::new(&request, &context).expect("valid transfer must build");

        assert_eq!(transaction.chain_id, context.chain_id);
        assert_eq!(transaction.nonce, context.nonce);
        assert_eq!(transaction.from, context.from);
        assert_eq!(transaction.to, Some(request.target().clone()));
        assert_eq!(transaction.value, request.value());
        assert_eq!(transaction.input, request.input());
        assert_eq!(transaction.gas_limit, context.gas_limit);
        assert_eq!(transaction.max_fee_per_gas, context.max_fee_per_gas);
        assert_eq!(
            transaction.max_priority_fee_per_gas,
            context.max_priority_fee_per_gas
        );
    }

    #[test]
    fn build_rejects_invalid_context_before_constructing_a_transaction() {
        let mut invalid_chain = context();
        invalid_chain.chain_id = 0;
        let mut invalid_gas = context();
        invalid_gas.gas_limit = 0;
        let mut invalid_fee = context();
        invalid_fee.max_fee_per_gas = Wei::from_u128(2);
        for (context, message) in [
            (invalid_chain, "Ethereum chain ID must be non-zero"),
            (invalid_gas, "Ethereum gas limit must be non-zero"),
            (
                invalid_fee,
                "Ethereum max fee per gas is below the priority fee",
            ),
        ] {
            let error = UnsignedTransaction::new(&request(), &context)
                .expect_err("invalid context must fail");
            assert_eq!(error.kind, ErrorKind::InvalidTransaction);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn build_preserves_erc20_call_without_sending_native_value() {
        let recipient = Address::from_bytes([0x22; 20]);
        let token = Address::from_bytes([0x33; 20]);
        let amount = Wei::from_u128(7);
        let request = Transfer {
            asset: crate::native::AssetKind::Erc20(token.clone()),
            to: recipient.clone(),
            amount: amount.clone(),
        };
        let transaction = UnsignedTransaction::new(&request, &context())
            .expect("valid token transfer must build");

        assert_eq!(transaction.to, Some(token));
        assert_eq!(transaction.value, Wei::ZERO);
        assert_eq!(transaction.input, request.input());
    }
}

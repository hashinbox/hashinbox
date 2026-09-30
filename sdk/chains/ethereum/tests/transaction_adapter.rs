//! An external transaction adapter can drive the native send pipeline:
//! inspect the typed [`Transfer`], supply the [`BuildContext`], and sign.

use base::BoxFuture;
use chain_ethereum::{
    Address, AssetKind, Error, FeeLimits, KeyPair, Wei,
    native::transaction::{BoundTransfer, BuildContext, Preflight, Transfer},
};
use crypto::SecretBytes;

struct InspectingAdapter;

impl Preflight for InspectingAdapter {
    fn context<'a>(
        &'a self,
        from: &'a Address,
        transfer: &'a Transfer,
        nonce: u64,
        limits: &'a FeeLimits,
    ) -> BoxFuture<'a, Result<BuildContext, Error>> {
        Box::pin(async move {
            assert_eq!(limits, &FeeLimits::default());
            assert!(!from.is_zero());
            assert_eq!(transfer.to, Address::from_bytes([0x22; 20]));
            match &transfer.asset {
                AssetKind::Native => {
                    assert_eq!(transfer.amount, Wei::from_u128(7));
                    assert_eq!(nonce, 1);
                }
                AssetKind::Erc20(token) => {
                    assert_eq!(token, &Address::from_bytes([0x33; 20]));
                    assert_eq!(transfer.amount, Wei::from_u128(9));
                    assert_eq!(nonce, 2);
                }
            }
            Ok(BuildContext {
                chain_id: 31_337,
                from: from.clone(),
                nonce,
                gas_limit: 21_000,
                max_fee_per_gas: Wei::from_u128(2),
                max_priority_fee_per_gas: Wei::from_u128(1),
            })
        })
    }
}

#[test]
fn external_transaction_adapter_can_inspect_typed_transfers_and_sign() {
    let key = KeyPair::import(SecretBytes::new([1; 32])).expect("valid scalar");
    let from = key.address().clone();
    let native = Transfer {
        asset: AssetKind::Native,
        to: Address::from_bytes([0x22; 20]),
        amount: Wei::from_u128(7),
    };
    let erc20 = Transfer {
        asset: AssetKind::Erc20(Address::from_bytes([0x33; 20])),
        to: Address::from_bytes([0x22; 20]),
        amount: Wei::from_u128(9),
    };
    let limits = FeeLimits::default();
    for (transfer, nonce) in [(native, 1), (erc20, 2)] {
        let context =
            futures_executor::block_on(InspectingAdapter.context(&from, &transfer, nonce, &limits))
                .expect("typed transfer must be inspectable");
        assert_eq!(context.nonce, nonce);
        let signed = futures_executor::block_on(BoundTransfer::new(transfer, context).sign(&key))
            .expect("the local key signs its own transfer");
        let Ok(fees) = signed.inspect_eip1559_fees() else {
            panic!("the signed envelope must be EIP-1559");
        };
        assert_eq!((fees.chain_id, fees.gas_limit), (31_337, 21_000));
    }
}

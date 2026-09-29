//! An external transaction adapter can drive the native send pipeline:
//! supply the node context through its own [`Preflight`], build the typed
//! [`Transfer`], sign, and inspect the signed envelope.

use base::BoxFuture;
use chain_solana::{
    Address, Error, KeyPair, Lamport,
    native::{
        Memo,
        rpc::Reading,
        transaction::{Lifetime, Preflight, Transfer, UnsignedTransaction},
    },
};
use crypto::SecretBytes;
use solana_hash::Hash;

struct InspectingAdapter;

impl Preflight for InspectingAdapter {
    fn lifetime(&self, min_slot: u64) -> BoxFuture<'_, Result<Reading<Lifetime>, Error>> {
        Box::pin(async move {
            Ok(Reading {
                slot: min_slot + 1,
                value: Lifetime::new(Hash::new_from_array([9; 32]), 44),
            })
        })
    }

    fn fee<'a>(
        &'a self,
        message: &'a [u8],
        min_slot: u64,
    ) -> BoxFuture<'a, Result<Reading<Lamport>, Error>> {
        Box::pin(async move {
            assert!(!message.is_empty());
            Ok(Reading {
                slot: min_slot,
                value: Lamport::from_atomic(5_000),
            })
        })
    }

    fn simulate<'a>(
        &'a self,
        transaction: &'a [u8],
        min_slot: u64,
    ) -> BoxFuture<'a, Result<u64, Error>> {
        Box::pin(async move {
            assert!(!transaction.is_empty());
            Ok(min_slot)
        })
    }
}

#[test]
fn external_adapter_prices_signs_and_simulates_a_typed_transfer() {
    futures_executor::block_on(async {
        let key = KeyPair::import(SecretBytes::new([7; 32])).expect("valid seed");
        let recipient = KeyPair::import(SecretBytes::new([8; 32]))
            .expect("valid seed")
            .address()
            .clone();
        let transfer =
            Transfer::new(recipient.clone(), Lamport::from_atomic(40_000)).expect("on-curve");
        let adapter = InspectingAdapter;
        let lifetime = adapter.lifetime(10).await.expect("lifetime");
        let message = UnsignedTransaction::new(
            key.address(),
            &transfer,
            Memo::from_bytes([3; Memo::LENGTH]),
            &lifetime.value,
        );
        let fee = adapter
            .fee(&message.bytes().expect("encodes"), lifetime.slot)
            .await
            .expect("fee");
        assert_eq!(fee.value, Lamport::from_atomic(5_000));
        let signed = message
            .sign(key.address(), lifetime.value, fee.slot, &key)
            .await
            .expect("the local key signs its own transfer");
        assert_eq!(&signed.from, key.address());
        assert_eq!(signed.min_slot, 11);
        let decoded: solana_transaction::Transaction =
            bincode::deserialize(signed.envelope.as_bytes()).expect("exact wire bytes");
        assert_eq!(decoded.signatures[0], *signed.id.signature());
        assert_eq!(
            Address::from_bytes(decoded.message.account_keys[1].to_bytes()),
            recipient
        );
        assert_eq!(
            adapter.simulate(signed.envelope.as_bytes(), 11).await,
            Ok(11)
        );
    });
}

//! An external transaction adapter can drive the native send pipeline:
//! supply the [`BuildContext`] for typed [`Transfer`]s, build, and sign.

use base::transaction::Envelope;
use chain_bitcoin::{
    Address, AddressType, FeeRate, KeyPair, Network, Satoshi,
    native::{
        SignedTransaction, TransactionId,
        block::{Outpoint, Output, Utxo},
        transaction::{BuildContext, Builder, Transfer},
    },
};
use crypto::SecretBytes;

/// What an adapter knows before building: its own funded output.
fn context(owner: &Address) -> BuildContext {
    let utxo = Utxo {
        outpoint: Outpoint {
            transaction_id: TransactionId([7; 32]),
            output_index: 1,
        },
        output: Output {
            value: Satoshi::from_atomic(100_000),
            script_pubkey: owner
                .script_pubkey(Network::Regtest)
                .expect("own address")
                .into_bytes(),
            address: Some(owner.clone()),
        },
    };
    BuildContext {
        network: Network::Regtest,
        available: vec![utxo],
        change: owner.clone(),
        fee_rate: FeeRate::from_sat_per_vb(2),
    }
}

#[test]
fn external_adapter_builds_signs_and_inspects_typed_transfers() {
    for kind in [AddressType::SegwitV0, AddressType::Taproot] {
        let key = KeyPair::import(SecretBytes::new([1; 32])).expect("valid scalar");
        let owner = key.address(Network::Regtest, kind);
        let recipient = KeyPair::import(SecretBytes::new([2; 32]))
            .expect("valid scalar")
            .address(Network::Regtest, AddressType::SegwitV0);
        let transfer = Transfer::new(recipient.clone(), Satoshi::from_atomic(40_000));
        let builder = Builder::new(vec![transfer], context(&owner));
        let unsigned = builder.build().expect("funded transfer builds");
        assert_eq!(unsigned.inputs[0].utxo.outpoint.output_index, 1);
        let signed = futures_executor::block_on(builder.sign(&key)).expect("own key signs");
        let restored = SignedTransaction::from_envelope(
            signed.id,
            Envelope::new(signed.envelope.as_bytes().to_vec()),
        )
        .expect("envelope decodes to its id");
        let view = restored
            .transaction(Network::Regtest)
            .expect("retained bytes decode");
        assert_eq!(view.outputs[0].address.as_ref(), Some(&recipient));
        assert_eq!(view.outputs[0].value, Satoshi::from_atomic(40_000));
        assert_eq!(view.outputs[1].address.as_ref(), Some(&owner));
    }
}
